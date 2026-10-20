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
    assert_eq!(zeron_engine::EngineProfile::scaffold(dir.path(), &auth).unwrap().store_root(), dir.path().join("profiles/scaffold").join(&key));
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

#[tokio::test]
async fn sandbox_storage_survives_epoch_change_without_reusing_authority() {
    use zeron_engine::{Engine, EngineConfig, EngineProfile, HarnessId};
    use zeron_sync::{DocsStore, scaffold_auth::{DeviceAuthority, DeviceConfig, profile_key}};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let other_origin = origin.replace("127.0.0.1", "localhost");
    let grant_id = "0123456789abcdef0123456789abcdef";
    let first = ScaffoldSession {
        access_token: format!("cs1.{grant_id}.current"), resource: origin.clone(), edge_origin: origin.clone(),
        identity: ScaffoldIdentity { user_id:"owner".into(), email:"owner@example.com".into(), name:None, project_scope:"canary".into(), deployment_id:Some("deployment".into()), capabilities:vec!["session.read".into()] },
        device: Some(DeviceAuthority { grant_id:grant_id.into(), expires_at:chrono::Utc::now().timestamp_millis() + 3_600_000, device_id:"device-e8".into(), session_id:"session".into(), deployment_id:"deployment".into(), sandbox_id:"sandbox".into(), lifecycle_epoch:8 }),
    };
    let authority = Arc::new(std::sync::Mutex::new(first.clone()));
    let server_authority = authority.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let size = stream.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..size]).starts_with("POST /auth/device-token/refresh "));
            let session = server_authority.lock().unwrap().clone();
            let device = session.device.unwrap();
            let body = serde_json::json!({
                "accessToken":session.access_token, "expiresAt":device.expires_at,
                "userId":session.identity.user_id, "email":session.identity.email,
                "projectId":session.identity.project_scope, "deploymentId":device.deployment_id,
                "sandboxId":device.sandbox_id, "targetDeviceId":device.device_id,
                "sessionId":device.session_id, "lifecycleEpoch":device.lifecycle_epoch,
                "capabilities":session.identity.capabilities,
            }).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("profiles/scaffold").join(profile_key(&first));
    let request = br#"{"principalId":"owner","control":{"type":"inspect"}}"#;
    let upload = legacy.join("uploads/retained.txt");
    let outcome_bytes = serde_json::to_vec(&serde_json::json!({"commandId":"done", "status":"confirmed", "attachment":upload})).unwrap();
    let outcome = outcome_bytes.as_slice();
    {
        let store = DocsStore::open(&legacy).unwrap();
        assert!(store.claim_crew_command("done", request).unwrap());
        store.finish_crew_command("done", outcome).unwrap();
        assert!(store.claim_crew_command("pending", request).unwrap());
    }
    std::fs::create_dir_all(upload.parent().unwrap()).unwrap();
    std::fs::write(&upload, b"absolute upload path survives").unwrap();
    let mut original_profile = None;
    let mut isolated_roots = std::collections::HashSet::new();
    let mut resumed = first.clone();
    resumed.device.as_mut().unwrap().device_id = "device-e9".into();
    resumed.device.as_mut().unwrap().lifecycle_epoch = 9;
    for field in ["first", "epoch", "owner", "resource", "edge", "project", "deployment", "session", "sandbox"] {
        let mut session = if field == "first" { first.clone() } else { resumed.clone() };
        match field {
            "owner" => session.identity.user_id = "other-owner".into(),
            "resource" => session.resource = other_origin.clone(),
            "edge" => session.edge_origin = other_origin.clone(),
            "project" => session.identity.project_scope = "other-project".into(),
            "deployment" => {
                session.identity.deployment_id = Some("other-deployment".into());
                session.device.as_mut().unwrap().deployment_id = "other-deployment".into();
            }
            "session" => session.device.as_mut().unwrap().session_id = "other-session".into(),
            "sandbox" => session.device.as_mut().unwrap().sandbox_id = "other-sandbox".into(),
            _ => {}
        }
        *authority.lock().unwrap() = session.clone();
        // A checkpoint credential alone is never trusted: the local authority
        // must refresh/verify it before profile resolution below can succeed.
        save_session(&dir.path().join("scaffold-device-session.json"), &session).unwrap();
        let device = session.device.as_ref().unwrap();
        let mut config = AuthConfig::new(&session.edge_origin, dir.path());
        config.scaffold = Some(ScaffoldConfig::new(&session.resource, &session.edge_origin, &session.identity.project_scope));
        config.device = Some(DeviceConfig {
            join_grant:String::new(), device_id:device.device_id.clone(), session_id:device.session_id.clone(),
            deployment_id:device.deployment_id.clone(), sandbox_id:device.sandbox_id.clone(), lifecycle_epoch:device.lifecycle_epoch,
        });
        let auth = Auth::detect(config.clone()).await;
        assert!(auth.state().is_signed_in(), "{field}");
        let profile = EngineProfile::scaffold(dir.path(), &auth).unwrap();
        assert_eq!(profile.scaffold_profile_key(), Some(profile_key(&session).as_str()));
        assert_eq!(profile.device_id_override(), Some(device.device_id.as_str()));
        if field != "epoch" {
            assert!(isolated_roots.insert(profile.store_root().to_path_buf()), "{field}");
        }
        let store = DocsStore::open(profile.store_root()).unwrap();
        if field == "first" || field == "epoch" {
            assert_eq!(profile.store_root(), legacy);
            assert_eq!(std::fs::read(&upload).unwrap(), b"absolute upload path survives");
            assert_eq!(store.crew_command("done").unwrap(), Some((request.to_vec(), Some(outcome.to_vec()))));
            assert_eq!(store.crew_command("pending").unwrap(), Some((request.to_vec(), None)));
            assert!(!store.claim_crew_command("pending", request).unwrap(), "ambiguous commands cannot replay after resume");
        } else {
            assert_ne!(profile.store_root(), legacy, "{field}");
            assert!(store.crew_commands().unwrap().is_empty(), "{field}");
            assert!(!profile.uploads_root().join("retained.txt").exists(), "{field}");
        }
        if field == "first" {
            original_profile = Some(profile);
        } else if field == "epoch" {
            let old = original_profile.as_ref().unwrap();
            assert_ne!(old.scaffold_profile_key(), profile.scaffold_profile_key());
            let runtime_config = EngineConfig {
                data_dir:dir.path().into(), edge_url:session.edge_origin.clone(), edge_token:None,
                ipc_port:0, default_harness:HarnessId::Mock, org_id:None, workos_client_id:None,
                scaffold:config.scaffold, device:config.device, managed_inference:false,
                device_bootstrap_file:None, scaffold_runtime_artifact:None,
            };
            assert!(Engine::assemble_runtime(&runtime_config, auth.clone(), old.clone()).await.is_err(), "old auth fingerprint must be rejected even with identical storage");
            let binding = std::fs::read_dir(legacy.parent().unwrap()).unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.file_name().unwrap().to_string_lossy().ends_with(".store.json"))
                .unwrap();
            let binding_bytes = std::fs::read(&binding).unwrap();
            let oversized = vec![b' '; 4097];
            std::fs::write(&binding, &oversized).unwrap();
            assert!(EngineProfile::scaffold(dir.path(), &auth).is_err());
            assert_eq!(std::fs::read(&binding).unwrap(), oversized);
            std::fs::write(&binding, &binding_bytes).unwrap();
            #[cfg(unix)]
            {
                let target = dir.path().join("binding-link-target.json");
                std::fs::write(&target, &binding_bytes).unwrap();
                std::fs::remove_file(&binding).unwrap();
                std::os::unix::fs::symlink(&target, &binding).unwrap();
                assert!(EngineProfile::scaffold(dir.path(), &auth).is_err());
                assert_eq!(std::fs::read(&target).unwrap(), binding_bytes);
                std::fs::remove_file(&binding).unwrap();
                std::fs::write(&binding, &binding_bytes).unwrap();
            }
            assert_eq!(EngineProfile::scaffold(dir.path(), &auth).unwrap().store_root(), legacy);
            assert_eq!(std::fs::read(&upload).unwrap(), b"absolute upload path survives");
            assert_eq!(store.crew_command("done").unwrap(), Some((request.to_vec(), Some(outcome.to_vec()))));
            // A second epoch-keyed store must never be merged or hidden by the binding.
            let competing = dir.path().join("profiles/scaffold").join(profile_key(&session));
            std::fs::create_dir(&competing).unwrap();
            std::fs::write(competing.join("outcome"), b"different state").unwrap();
            assert!(EngineProfile::scaffold(dir.path(), &auth).is_err());
            assert_eq!(std::fs::read(competing.join("outcome")).unwrap(), b"different state");
        }
    }
    server.abort();
}

#[cfg(unix)]
mod terminal_login {
    use super::*;
    use std::os::fd::FromRawFd;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncBufReadExt;

    #[tokio::test]
    async fn child() {
        let Some(origin) = std::env::var_os("CREW_TERMINAL_LOGIN_TEST_ORIGIN") else {
            return;
        };
        let origin = origin.to_str().unwrap();
        let dir = std::env::var_os("CREW_TERMINAL_LOGIN_TEST_DIR").unwrap();
        let mut config = AuthConfig::new(origin, std::path::Path::new(&dir));
        config.scaffold = Some(ScaffoldConfig::new(origin, origin, "canary"));
        let auth = Auth::new(config);
        zeron_engine::terminal_sign_in(&auth).await.unwrap();
        assert!(matches!(auth.state(), AuthState::SignedIn { user, .. }
            if user.email == "terminal-test@example.com"));
        println!("Terminal sign-in completed.");
        // Normal test-runtime destruction must also finish, not just the future.
    }

    async fn serve_authority(listener: tokio::net::TcpListener, origin: String) {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = tokio::io::BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let target = line.split_whitespace().nth(1).unwrap().to_string();
            let mut content_length = 0;
            loop {
                line.clear();
                assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    content_length = value.trim().parse::<usize>().unwrap();
                }
            }
            stream.read_exact(&mut vec![0; content_length]).await.unwrap();
            let scopes = DEFAULT_OAUTH_SCOPES.split_whitespace().collect::<Vec<_>>();
            let body = match target.as_str() {
                "/.well-known/oauth-protected-resource" => serde_json::json!({
                    "resource": origin, "authorization_servers": [&origin], "scopes_supported": scopes,
                }),
                "/.well-known/oauth-authorization-server" => serde_json::json!({
                    "issuer": origin, "authorization_endpoint": format!("{origin}/authorize"),
                    "token_endpoint": format!("{origin}/token"),
                    "registration_endpoint": format!("{origin}/register"),
                    "code_challenge_methods_supported": ["S256"],
                }),
                "/register" => serde_json::json!({"client_id": "terminal-test-client"}),
                "/token" => serde_json::json!({
                    "access_token": "sc_rc_terminal_test", "token_type": "Bearer",
                    "scope": DEFAULT_OAUTH_SCOPES, "resource": origin,
                }),
                "/api/code-sandboxes/auth/session" => serde_json::json!({
                    "ok": true, "resource": origin,
                    "actor": {"sub": "terminal-test-user", "auth": "iap"}, "scopes": scopes,
                }),
                "/auth/session" => serde_json::json!({
                    "userId": "terminal-test-user", "email": "terminal-test@example.com",
                    "projectScope": "canary", "credential": "scaffold", "capabilities": ["session.read"],
                }),
                _ => panic!("unexpected authority request: {target}"),
            }.to_string();
            stream.get_mut().write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len(),
            ).as_bytes()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn scaffold_loopback_exits_with_stdin_open_and_silent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(serve_authority(listener, origin.clone()));
        let dir = tempfile::tempdir().unwrap();
        let (mut master, mut slave) = (-1, -1);
        // Only stdin needs a TTY; keep stdout piped for bounded async observation.
        assert_eq!(unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
        }, 0);
        // SAFETY: openpty returned two fresh, owned file descriptors above.
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "terminal_login::child", "--nocapture"])
            .env("CREW_TERMINAL_LOGIN_TEST_ORIGIN", &origin)
            .env("CREW_TERMINAL_LOGIN_TEST_DIR", dir.path())
            .stdin(Stdio::from(slave))
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn().unwrap();
        let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let mut transcript = String::new();
        let url = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let line = output.next_line().await.unwrap().expect("child sign-in URL");
                transcript.push_str(&line);
                transcript.push('\n');
                if line.trim().starts_with(&format!("{origin}/authorize?")) {
                    break reqwest::Url::parse(line.trim()).unwrap();
                }
            }
        }).await.expect("terminal sign-in should print its browser URL");
        let params = url.query_pairs().collect::<std::collections::HashMap<_, _>>();
        let mut callback = reqwest::Url::parse(&params["redirect_uri"]).unwrap();
        callback.query_pairs_mut().append_pair("state", &params["state"]).append_pair("code", "terminal-test-code");
        // Model time in the browser so an erroneous blocking reader can start.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let response = reqwest::Client::new().get(callback)
            .timeout(Duration::from_secs(5)).send().await.unwrap();
        assert!(response.status().is_success());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let line = output.next_line().await.unwrap().expect("child sign-in completion");
                transcript.push_str(&line);
                transcript.push('\n');
                if line == "Terminal sign-in completed." {
                    break;
                }
            }
        }).await.expect("loopback callback should complete terminal sign-in");
        let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            Ok(status) => status.unwrap(),
            Err(_) => {
                child.kill().await.unwrap();
                panic!("login returned but its Tokio runtime hung with stdin open: {transcript}");
            }
        };
        // No input, EOF, or extra newline was supplied before process exit.
        drop(master);
        server.abort();
        assert!(status.success(), "{status}: {transcript}");
    }
}
