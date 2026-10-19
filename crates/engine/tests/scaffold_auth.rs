//! Managed auth regression: cold launch, transient outages, revocation, and scope denial.
use std::sync::{Arc, atomic::{AtomicU16, Ordering}};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeron_engine::{Auth, AuthConfig, AuthState};
use zeron_rpc::TokenError;
use zeron_sync::scaffold_auth::{save_session, ScaffoldConfig, ScaffoldIdentity, ScaffoldSession, DEFAULT_OAUTH_SCOPES};

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
