//! Managed auth regression: cold launch, transient outages, revocation, and scope denial.
use std::sync::{Arc, atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering}};
use parking_lot::Mutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeron_engine::{Auth, AuthConfig, AuthState};
use zeron_rpc::TokenError;
use zeron_sync::scaffold_auth::{save_session, ScaffoldConfig, ScaffoldIdentity, ScaffoldSession, DEFAULT_OAUTH_SCOPES};

// Advance only between requests: paused Tokio auto-advance must not race real TCP.
async fn elapse(duration: Duration) {
    tokio::time::pause();
    tokio::time::advance(duration).await;
    tokio::time::resume();
}

#[tokio::test]
async fn persisted_identity_is_revalidated_and_revocation_is_not_an_outage() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let status = Arc::new(AtomicU16::new(200));
    let server_status = status.clone();
    let resource = origin.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let size = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.to_ascii_lowercase().contains("authorization: bearer sc_rc_test"));
            let code = server_status.load(Ordering::SeqCst);
            let body = if request.starts_with("GET /auth/session ") {
                serde_json::json!({"userId":"user@example.com", "email":"user@example.com", "projectScope":"canary", "credential":"scaffold", "capabilities":["session.read"]})
            } else {
                assert!(request.starts_with("GET /api/code-sandboxes/auth/session "));
                serde_json::json!({"ok":true, "resource":resource, "actor":{"sub":"user@example.com", "auth":"iap"}, "scopes":DEFAULT_OAUTH_SCOPES.split_whitespace().collect::<Vec<_>>()})
            }.to_string();
            let response = format!("HTTP/1.1 {code} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scaffold-session.json");
    let session = ScaffoldSession { access_token:"sc_rc_test".into(), identity:ScaffoldIdentity { user_id:"user@example.com".into(), email:"user@example.com".into(), name:None, project_scope:"canary".into(), capabilities:vec!["session.read".into()], deployment_id:None }, resource:origin.clone(), edge_origin:origin.clone(), device:None };
    save_session(&path, &session).unwrap();
    let mut config = AuthConfig::new(&origin, dir.path());
    config.scaffold = Some(ScaffoldConfig::new(&origin, &origin, "canary"));
    let unverified = Auth::new(config.clone());
    assert_eq!(unverified.state(), AuthState::SignedOut);
    assert!(unverified.verified_profile_key().is_none());
    let unbound = unverified.for_profile_tokens();
    assert_eq!(zeron_rpc::TokenSource::token(unbound.as_ref()).await, Err(TokenError::SignedOut), "an already-open local runtime cannot become a managed network profile");
    let auth = Auth::detect(config.clone()).await;
    assert!(auth.state().is_signed_in());
    let key = auth.verified_profile_key().unwrap();
    let bound = auth.for_profile_tokens();
    assert_eq!(zeron_rpc::TokenSource::token(bound.as_ref()).await.unwrap(), "sc_rc_test");
    status.store(503, Ordering::SeqCst);
    elapse(Duration::from_secs(60)).await;
    assert!(matches!(auth.access_token().await, Err(TokenError::TemporarilyUnavailable(_))));
    let client = zeron_engine::scaffold::ScaffoldClient::new(&origin, "canary", bound).unwrap();
    let scope = zeron_proto::CollaborationScope {
        project_id: "canary".into(), deployment_id: Some("deployment".into()), session_id: None, unknown: Default::default(),
    };
    let error = client.list(&scope, &tokio_util::sync::CancellationToken::new()).await.unwrap_err();
    let zeron_engine::scaffold::ScaffoldError::AuthTemporarilyUnavailable(reason) = error else {
        panic!("authority outage must remain retryable");
    };
    assert!(reason.contains("503"));
    assert!(!reason.contains(&origin));
    assert!(!reason.contains("sc_rc_test"));
    assert!(!reason.contains("user@example.com"));
    assert!(path.exists());
    assert_eq!(auth.verified_profile_key().as_deref(), Some(key.as_str()));
    status.store(401, Ordering::SeqCst);
    elapse(Duration::from_secs(6)).await;
    assert_eq!(auth.access_token().await, Err(TokenError::SignedOut));
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(!path.exists());
    assert!(auth.verified_profile_key().is_none());
    status.store(200, Ordering::SeqCst);
    // Even without an operator deployment pin, an existing identity cannot
    // silently follow authority into a different deployment on revalidation.
    let mut old_deployment = session.clone();
    old_deployment.identity.deployment_id = Some("previous-deployment".into());
    save_session(&path, &old_deployment).unwrap();
    let moved = Auth::detect(config.clone()).await;
    assert_eq!(moved.state(), AuthState::SignedOut);
    assert!(moved.verified_profile_key().is_none());
    assert!(!path.exists());
    save_session(&path, &session).unwrap();
    config.scaffold.as_mut().unwrap().project_scope = "other-project".into();
    let wrong_scope = Auth::detect(config).await;
    assert_eq!(wrong_scope.state(), AuthState::SignedOut);
    assert!(wrong_scope.verified_scope().is_none());
    assert!(!path.exists());
    server.abort();
}

#[tokio::test]
async fn absent_or_invalid_managed_configuration_never_enables_development() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = AuthConfig::new("https://edge.example", dir.path());
    let auth = Auth::detect(config.clone()).await;
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert_eq!(auth.access_token().await, Err(TokenError::SignedOut));
    config.allow_dev_auth = true;
    config.scaffold = Some(ScaffoldConfig::new("http://untrusted.example", "https://edge.example", "canary"));
    let auth = Auth::detect(config).await;
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(!auth.dev_enabled());
    assert!(matches!(auth.access_token().await, Err(TokenError::TemporarilyUnavailable(_))));
}

#[tokio::test]
async fn device_resume_enrolls_new_grant_and_restarts_preserve_its_authority() {
    use zeron_sync::scaffold_auth::{DeviceAuthority, DeviceConfig};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let id = "0123456789abcdef0123456789abcdef";
    let old_id = "00000000000000000000000000000000";
    let expires = chrono::Utc::now().timestamp_millis() + 12 * 60 * 60 * 1000;
    let server = tokio::spawn(async move {
        let mut previous = "";
        for secret in ["enrolled", "rotated", "restored"] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let size = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            if previous.is_empty() {
                assert!(request.starts_with("POST /auth/device-grants/exchange "));
                assert!(request.contains(&format!("cg1.{id}.grant")));
            } else {
                assert!(request.starts_with("POST /auth/device-token/refresh "));
                assert!(request.contains(&format!("cs1.{id}.{previous}")));
            }
            let body = serde_json::json!({"accessToken":format!("cs1.{id}.{secret}"),"expiresAt":expires,"userId":"owner","email":"owner@example.com","projectId":"canary","deploymentId":"deployment","sandboxId":"sandbox","targetDeviceId":"device-e9","sessionId":"session","lifecycleEpoch":9,"capabilities":["session.read"]}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            previous = secret;
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scaffold-device-session.json");
    let session = ScaffoldSession {
        access_token: format!("cs1.{old_id}.revoked"), resource: origin.clone(), edge_origin: origin.clone(),
        identity: ScaffoldIdentity { user_id:"owner".into(), email:"owner@example.com".into(), name:None, project_scope:"canary".into(), deployment_id:Some("deployment".into()), capabilities:vec!["session.read".into()] },
        device:Some(DeviceAuthority { grant_id:old_id.into(), expires_at:expires, device_id:"device-e8".into(), session_id:"session".into(), deployment_id:"deployment".into(), sandbox_id:"sandbox".into(), lifecycle_epoch:8 }),
    };
    save_session(&path, &session).unwrap();
    let mut config = AuthConfig::new(&origin, dir.path());
    config.scaffold = Some(ScaffoldConfig::new(&origin, &origin, "canary"));
    config.device = Some(DeviceConfig { join_grant:format!("cg1.{id}.grant"), device_id:"device-e9".into(), session_id:"session".into(), deployment_id:"deployment".into(), sandbox_id:"sandbox".into(), lifecycle_epoch:9 });
    for secret in ["enrolled", "rotated"] {
        let auth = Auth::detect(config.clone()).await;
        assert_eq!(auth.device_grant_authority().unwrap().lifecycle_epoch, 9);
        assert_eq!(auth.access_token().await.unwrap(), format!("cs1.{id}.{secret}"));
    }
    config.device = None;
    let auth = Auth::detect(config.clone()).await;
    assert!(auth.device_mode());
    assert_eq!(auth.device_grant_authority().unwrap().lifecycle_epoch, 9);
    assert_eq!(auth.access_token().await.unwrap(), format!("cs1.{id}.restored"));
    auth.sign_out();
    config.scaffold = None;
    config.allow_dev_auth = true;
    let restarted = Auth::new(config);
    assert!(restarted.device_mode());
    assert!(!restarted.dev_enabled());
    assert_eq!(restarted.state(), AuthState::SignedOut);
    assert!(!std::fs::read_to_string(&path).unwrap().contains("cs1."));
    server.await.unwrap();
}

struct Authority {
    control_status: AtomicU16,
    edge_status: AtomicU16,
    control_calls: AtomicUsize,
    edge_calls: AtomicUsize,
    retry_after: Mutex<String>,
    replacement_user: Mutex<&'static str>,
    hold: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct ManagedServer {
    origin: String,
    state: Arc<Authority>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ManagedServer {
    fn drop(&mut self) { self.task.abort(); }
}

impl ManagedServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Authority {
            control_status: AtomicU16::new(200), edge_status: AtomicU16::new(200),
            control_calls: AtomicUsize::new(0), edge_calls: AtomicUsize::new(0),
            retry_after: Mutex::new(String::new()), replacement_user: Mutex::new("replacement"),
            hold: AtomicBool::new(false), entered: tokio::sync::Notify::new(), release: tokio::sync::Notify::new(),
        });
        let server_state = state.clone();
        let resource = origin.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let state = server_state.clone();
                let resource = resource.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    loop {
                        let mut chunk = [0; 8192];
                        let size = stream.read(&mut chunk).await.unwrap();
                        assert!(size > 0);
                        request.extend_from_slice(&chunk[..size]);
                        if request.windows(4).any(|part| part == b"\r\n\r\n") { break; }
                    }
                    let request = String::from_utf8(request).unwrap();
                    let path = request.split_whitespace().nth(1).unwrap();
                    let user = if request.contains("Bearer sc_rc_new") {
                        *state.replacement_user.lock()
                    } else { "original" };
                    let scopes: Vec<_> = DEFAULT_OAUTH_SCOPES.split_whitespace().collect();
                    let mut code = 200;
                    let body = match path {
                        "/api/code-sandboxes/auth/session" => {
                            assert!(request.contains("Bearer sc_rc_"));
                            state.control_calls.fetch_add(1, Ordering::SeqCst);
                            code = state.control_status.load(Ordering::SeqCst);
                            if state.hold.swap(false, Ordering::SeqCst) {
                                state.entered.notify_one();
                                state.release.notified().await;
                            }
                            serde_json::json!({"ok":true,"resource":resource,"actor":{"sub":user,"auth":"iap"},"scopes":scopes})
                        }
                        "/auth/session" => {
                            state.edge_calls.fetch_add(1, Ordering::SeqCst);
                            code = state.edge_status.load(Ordering::SeqCst);
                            serde_json::json!({"userId":user,"email":format!("{user}@example.com"),"projectScope":"canary","credential":"scaffold","capabilities":["session.read"]})
                        }
                        "/.well-known/oauth-protected-resource" => serde_json::json!({"resource":resource,"authorization_servers":[resource],"scopes_supported":scopes}),
                        "/.well-known/oauth-authorization-server" => serde_json::json!({"issuer":resource,"authorization_endpoint":format!("{resource}/authorize"),"token_endpoint":format!("{resource}/token"),"registration_endpoint":format!("{resource}/register"),"code_challenge_methods_supported":["S256"]}),
                        "/register" => serde_json::json!({"client_id":"test-client"}),
                        "/token" => serde_json::json!({"access_token":"sc_rc_new","token_type":"Bearer","scope":DEFAULT_OAUTH_SCOPES,"resource":resource}),
                        _ => panic!("unexpected auth path: {path}"),
                    }.to_string();
                    let retry = state.retry_after.lock().clone();
                    let header = if retry.is_empty() { String::new() } else { format!("Retry-After: {retry}\r\n") };
                    stream.write_all(format!("HTTP/1.1 {code} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{header}Connection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                });
            }
        });
        Self { origin, state, task }
    }

    fn auth(&self, dir: &std::path::Path) -> Auth {
        save_session(&dir.join("scaffold-session.json"), &ScaffoldSession {
            access_token: "sc_rc_old".into(), resource: self.origin.clone(), edge_origin: self.origin.clone(), device: None,
            identity: ScaffoldIdentity { user_id: "original".into(), email: "original@example.com".into(), name: None, project_scope: "canary".into(), capabilities: vec!["session.read".into()], deployment_id: None },
        }).unwrap();
        let mut config = AuthConfig::new(&self.origin, dir);
        config.scaffold = Some(ScaffoldConfig::new(&self.origin, &self.origin, "canary"));
        Auth::new(config)
    }

    fn calls(&self) -> (usize, usize) {
        (self.state.control_calls.load(Ordering::SeqCst), self.state.edge_calls.load(Ordering::SeqCst))
    }
}

async fn consumers(auth: &Auth) -> Vec<Result<String, TokenError>> {
    let barrier = Arc::new(tokio::sync::Barrier::new(20));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let auth = auth.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move { barrier.wait().await; zeron_rpc::TokenSource::token(&auth).await });
    }
    let mut results = Vec::new();
    while let Some(result) = tasks.join_next().await { results.push(result.unwrap()); }
    results
}

async fn replace_credential(auth: &Auth) {
    let authorize = reqwest::Url::parse(&auth.start_sign_in().await.unwrap()).unwrap();
    let state = authorize.query_pairs().find(|(key, _)| key == "state").unwrap().1.into_owned();
    auth.complete_sign_in(&format!("{state}.test-code")).await.unwrap();
}

#[tokio::test]
async fn managed_concurrent_and_sequential_consumers_share_bounded_verification() {
    let server = ManagedServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = server.auth(dir.path());
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(auth.verified_profile_key().is_none());
    assert!(consumers(&auth).await.iter().all(|result| result.as_deref() == Ok("sc_rc_old")));
    let bound = auth.for_profile_tokens();
    for _ in 0..20 { assert_eq!(zeron_rpc::TokenSource::token(bound.as_ref()).await.unwrap(), "sc_rc_old"); }
    assert_eq!(server.calls(), (1, 1));
    elapse(Duration::from_secs(50)).await;
    assert_eq!(auth.access_token().await.unwrap(), "sc_rc_old");
    assert_eq!(server.calls(), (1, 1));
    elapse(Duration::from_secs(10)).await;
    assert!(consumers(&auth).await.iter().all(Result::is_ok));
    assert_eq!(server.calls(), (2, 2));
}

#[tokio::test]
async fn managed_429_and_503_share_retry_after_and_resume_after_cooldown() {
    for edge_failure in [false, true] {
        let server = ManagedServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let auth = server.auth(dir.path());
        // One case starts unverified; the other proves expired success is not grace.
        if edge_failure {
            auth.access_token().await.unwrap();
            elapse(Duration::from_secs(60)).await;
            server.state.edge_status.store(503, Ordering::SeqCst);
            *server.state.retry_after.lock() = (chrono::Utc::now() + chrono::TimeDelta::seconds(30)).to_rfc2822();
        } else {
            server.state.control_status.store(429, Ordering::SeqCst);
            *server.state.retry_after.lock() = "30".into();
        }
        let results = consumers(&auth).await;
        assert!(matches!(&results[0], Err(TokenError::TemporarilyUnavailable(_))));
        assert!(results.iter().all(|result| result == &results[0]));
        let calls = if edge_failure { (2, 2) } else { (1, 0) };
        assert_eq!(server.calls(), calls);
        assert!(dir.path().join("scaffold-session.json").exists());
        assert_eq!(auth.state().is_signed_in(), edge_failure);
        server.state.control_status.store(200, Ordering::SeqCst);
        server.state.edge_status.store(200, Ordering::SeqCst);
        auth.retry_refresh(); // connectivity hints must not defeat Retry-After
        elapse(Duration::from_secs(20)).await;
        assert_eq!(auth.access_token().await, results[0]);
        assert_eq!(server.calls(), calls);
        elapse(Duration::from_secs(11)).await;
        assert!(consumers(&auth).await.iter().all(Result::is_ok));
        assert_eq!(server.calls(), (calls.0 + 1, calls.1 + 1));
        assert!(auth.state().is_signed_in());
    }
}

#[tokio::test]
async fn managed_signout_fences_inflight_success_and_failure() {
    for status in [200, 429, 503] {
        let server = ManagedServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let auth = server.auth(dir.path());
        auth.access_token().await.unwrap();
        elapse(Duration::from_secs(60)).await;
        server.state.control_status.store(status, Ordering::SeqCst);
        server.state.hold.store(true, Ordering::SeqCst);
        let pending = tokio::spawn({ let auth = auth.clone(); async move { auth.access_token().await } });
        server.state.entered.notified().await;
        auth.sign_out();
        server.state.release.notify_one();
        assert_eq!(pending.await.unwrap(), Err(TokenError::SignedOut));
        assert_eq!(auth.access_token().await, Err(TokenError::SignedOut));
        assert_eq!(auth.state(), AuthState::SignedOut);
        assert!(auth.verified_profile_key().is_none());
        assert!(!dir.path().join("scaffold-session.json").exists());
        server.state.control_status.store(200, Ordering::SeqCst);
        replace_credential(&auth).await;
        assert_eq!(auth.access_token().await.unwrap(), "sc_rc_new");
    }
}

#[tokio::test]
async fn managed_replacement_fences_old_success_rejection_and_cooldown() {
    for status in [200, 401, 503] {
        for user in ["original", "replacement"] {
            let server = ManagedServer::start().await;
            *server.state.replacement_user.lock() = user;
            let dir = tempfile::tempdir().unwrap();
            let auth = server.auth(dir.path());
            auth.access_token().await.unwrap();
            let bound = auth.for_profile_tokens();
            elapse(Duration::from_secs(60)).await;
            server.state.control_status.store(status, Ordering::SeqCst);
            server.state.hold.store(true, Ordering::SeqCst);
            let pending = auth.access_token();
            tokio::pin!(pending);
            tokio::select! {
                _ = server.state.entered.notified() => {},
                result = &mut pending => panic!("verification escaped held response: {result:?}"),
            }
            server.state.release.notify_one();
            // Let the detached flight finish, but leave this consumer unpolled
            // until OAuth replaces its credential/profile. Its old result must
            // not be returned as authority or revoke the replacement.
            let _ = auth.access_token().await;
            server.state.control_status.store(200, Ordering::SeqCst);
            replace_credential(&auth).await;
            assert!(matches!(pending.await, Err(TokenError::TemporarilyUnavailable(_))));
            assert_eq!(auth.user_id().as_deref(), Some(user));
            let result = zeron_rpc::TokenSource::token(bound.as_ref()).await;
            if user == "original" { assert_eq!(result.unwrap(), "sc_rc_new"); }
            else { assert_eq!(result, Err(TokenError::SignedOut)); }
            elapse(Duration::from_secs(60)).await;
            assert_eq!(auth.access_token().await.unwrap(), "sc_rc_new");
            assert!(dir.path().join("scaffold-session.json").exists());
        }
    }
}

#[tokio::test]
async fn managed_cancelled_consumer_does_not_cancel_shared_verification() {
    let server = ManagedServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = server.auth(dir.path());
    server.state.hold.store(true, Ordering::SeqCst);
    let pending = tokio::spawn({ let auth = auth.clone(); async move { auth.access_token().await } });
    server.state.entered.notified().await;
    pending.abort();
    server.state.release.notify_one();
    assert_eq!(auth.access_token().await.unwrap(), "sc_rc_old");
    assert_eq!(server.calls(), (1, 1));
}

#[tokio::test]
async fn managed_explicit_rejection_clears_cached_authority() {
    for edge_failure in [false, true] {
        let server = ManagedServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let auth = server.auth(dir.path());
        auth.access_token().await.unwrap();
        let bound = auth.for_profile_tokens();
        elapse(Duration::from_secs(60)).await;
        if edge_failure { server.state.edge_status.store(403, Ordering::SeqCst); }
        else { server.state.control_status.store(401, Ordering::SeqCst); }
        assert!(consumers(&auth).await.iter().all(|result| *result == Err(TokenError::SignedOut)));
        let calls = server.calls();
        server.state.control_status.store(200, Ordering::SeqCst);
        server.state.edge_status.store(200, Ordering::SeqCst);
        assert_eq!(zeron_rpc::TokenSource::token(bound.as_ref()).await, Err(TokenError::SignedOut));
        assert_eq!(server.calls(), calls);
        assert_eq!(auth.state(), AuthState::SignedOut);
        assert!(auth.verified_scope().is_none());
        assert!(!dir.path().join("scaffold-session.json").exists());
    }
}

#[tokio::test]
async fn managed_headerless_failures_share_local_backoff() {
    for status in [429, 503] {
        let server = ManagedServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let auth = server.auth(dir.path());
        server.state.control_status.store(status, Ordering::SeqCst);
        let failures = consumers(&auth).await;
        assert!(matches!(&failures[0], Err(TokenError::TemporarilyUnavailable(_))));
        assert!(failures.iter().all(|failure| failure == &failures[0]));
        assert_eq!(auth.access_token().await, failures[0]);
        assert_eq!(server.calls(), (1, 0));
        server.state.control_status.store(200, Ordering::SeqCst);
        elapse(Duration::from_secs(6)).await;
        assert!(consumers(&auth).await.iter().all(Result::is_ok));
        assert_eq!(server.calls(), (2, 1));
    }
}

#[tokio::test]
async fn managed_replacement_clears_previous_retry_window() {
    let server = ManagedServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = server.auth(dir.path());
    server.state.control_status.store(429, Ordering::SeqCst);
    *server.state.retry_after.lock() = "600".into();
    assert!(matches!(auth.access_token().await, Err(TokenError::TemporarilyUnavailable(_))));
    server.state.control_status.store(200, Ordering::SeqCst);
    replace_credential(&auth).await;
    assert_eq!(auth.user_id().as_deref(), Some("replacement"));
    assert_eq!(server.calls(), (2, 1));
    elapse(Duration::from_secs(60)).await;
    assert_eq!(auth.access_token().await.unwrap(), "sc_rc_new");
    assert_eq!(server.calls(), (3, 2));
}
