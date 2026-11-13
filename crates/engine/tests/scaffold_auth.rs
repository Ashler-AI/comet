//! Crew: Scaffold sign-in in the engine (docs/crew/zgx-thin-port.md, slice 2).
//! Cold launch, transient outages, revocation, scope denial, device grants,
//! what the RPC surface answers in Scaffold mode, and no dev-auth fallthrough.
//!
//! Ported from Crew's `crates/engine/tests/scaffold_auth.rs`. Every authority
//! is a loopback stub. Crew's cases that drive the rejection fence
//! (`AccessToken { generation }`, `TokenSource::invalidate`) wait for it: here
//! re-verification comes from a sign-out, a replacement or a restart instead.
//! Crew's sandbox-runtime cases (`ScaffoldClient`, crew commands) are slice 6.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeron_engine::{
    Auth, AuthConfig, AuthState, Engine, EngineConfig, EngineCore, EngineProfile, HarnessId,
    HarnessRegistry, WorkspaceScope,
};
use zeron_rpc::{RpcError, TokenError, TokenSource, methods};
use zeron_sync::scaffold_auth::{
    DEFAULT_OAUTH_SCOPES, ScaffoldConfig, ScaffoldIdentity, ScaffoldSession, save_session,
};

// Advance only between requests: paused Tokio auto-advance must not race real TCP.
async fn elapse(duration: Duration) {
    tokio::time::pause();
    tokio::time::advance(duration).await;
    tokio::time::resume();
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn engine_config(dir: &std::path::Path, edge: &str, scaffold: Option<ScaffoldConfig>) -> EngineConfig {
    EngineConfig {
        data_dir: dir.to_path_buf(),
        edge_url: edge.into(),
        edge_token: None,
        ipc_port: 0,
        default_harness: HarnessId::Mock,
        org_id: None,
        workos_client_id: None,
        scaffold,
        device: None,
    }
}

#[tokio::test]
async fn persisted_identity_is_revalidated_and_revocation_is_not_an_outage() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let status = Arc::new(AtomicU16::new(200));
    let server_status = status.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let server_calls = calls.clone();
    let resource = origin.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let size = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.to_ascii_lowercase().contains("authorization: bearer sc_rc_test"));
            let code = server_status.load(Ordering::SeqCst);
            server_calls.fetch_add(1, Ordering::SeqCst);
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
    assert_eq!(TokenSource::token(unbound.as_ref()).await, Err(TokenError::SignedOut), "an already-open local runtime cannot become a managed network profile");
    let auth = Auth::detect(config.clone()).await;
    assert!(matches!(auth.state(), AuthState::SignedIn { user, org_id: Some(org) }
        if user.id == "user@example.com" && org == "canary"), "SignedIn {{ orgId = projectScope }}");
    let key = auth.verified_profile_key().unwrap();
    assert_eq!(EngineProfile::scaffold(dir.path(), &auth).unwrap().store_root(), dir.path().join("profiles/scaffold").join(&key));
    // The unbound profile above verified through a separate cold Auth instance.
    let verified_calls = calls.load(Ordering::SeqCst);
    let bound = auth.for_profile_tokens();
    assert_eq!(TokenSource::token(bound.as_ref()).await.unwrap(), "sc_rc_test");
    let tokens = futures::future::join_all((0..16).map(|_| auth.access_token())).await;
    assert!(tokens.iter().all(|token| token.as_deref() == Ok("sc_rc_test")));
    assert_eq!(calls.load(Ordering::SeqCst), verified_calls, "warm engine token reads do not reverify");
    status.store(503, Ordering::SeqCst);
    assert_eq!(auth.access_token().await.unwrap(), "sc_rc_test", "fresh verification is reused during an outage");
    // A start during the outage keeps the credential, its verification pending.
    let outage = Auth::detect(config.clone()).await;
    assert_eq!(outage.state(), AuthState::SignedOut);
    assert!(outage.pending_scaffold_verification());
    assert!(matches!(outage.access_token().await, Err(TokenError::TemporarilyUnavailable(_))));
    assert!(path.exists());
    // Revocation is not an outage: the next start signs out and forgets it.
    status.store(401, Ordering::SeqCst);
    let revoked = Auth::detect(config.clone()).await;
    assert_eq!(revoked.access_token().await, Err(TokenError::SignedOut));
    assert_eq!(revoked.state(), AuthState::SignedOut);
    assert!(!revoked.pending_scaffold_verification());
    assert!(!path.exists());
    assert!(revoked.verified_profile_key().is_none());
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
    let auth = Auth::detect(config.clone()).await;
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert_eq!(Engine::initial_workspace_scope(&auth), WorkspaceScope::Local);
    assert!(!auth.pending_scaffold_verification());
    assert!(!auth.dev_enabled());
    assert!(matches!(auth.access_token().await, Err(TokenError::TemporarilyUnavailable(_))));
    assert!(auth.start_sign_in().await.is_err(), "no sign-in through an authority it can't trust");

    save_session(&dir.path().join("scaffold-session.json"), &ScaffoldSession {
        access_token: "sc_rc_retained".into(), resource: "https://control.example".into(),
        edge_origin: "https://edge.example".into(), device: None,
        identity: ScaffoldIdentity { user_id: "retained".into(), email: "retained@example.com".into(),
            name: None, project_scope: "canary".into(), capabilities: vec![], deployment_id: None },
    }).unwrap();
    let credential = std::fs::read(dir.path().join("scaffold-session.json")).unwrap();
    let engine_config = engine_config(dir.path(), &config.edge_url, config.scaffold.clone());
    let retained = Auth::detect(config).await;
    let scope = Engine::initial_workspace_scope(&retained);
    assert_eq!(scope, WorkspaceScope::Synced);
    assert!(Engine::resolve_profile(&engine_config, &retained, scope).is_err());
    assert!(retained.loaded_scaffold_session());
    assert!(!retained.pending_scaffold_verification());
    assert_eq!(retained.state(), AuthState::SignedOut);
    assert!(retained.verified_scaffold_session().is_none());
    assert_eq!(std::fs::read(dir.path().join("scaffold-session.json")).unwrap(), credential);
    assert!(!dir.path().join("local-profile.json").exists());
}

/// A development bearer (`ZERON_EDGE_TOKEN`) never stands in for, or next
/// to, a Scaffold authority: the engine is in Scaffold mode, signed out.
#[tokio::test]
async fn a_development_bearer_never_signs_a_scaffold_engine_in() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = engine_config(
        dir.path(),
        "http://127.0.0.1:9",
        Some(ScaffoldConfig::new("http://127.0.0.1:9", "http://127.0.0.1:9", "canary")),
    );
    config.edge_token = Some("dev-user@dev-org".into());
    config.workos_client_id = Some("client_test".into());
    let auth = Engine::build_auth(&config).await;
    assert!(auth.scaffold_enabled() && !auth.dev_enabled() && !auth.workos_enabled());
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert_eq!(auth.user_id(), None);
    assert_eq!(auth.access_token().await, Err(TokenError::SignedOut));
    assert_eq!(Engine::initial_workspace_scope(&auth), WorkspaceScope::Local);
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
    assert!(auth.start_sign_in().await.is_err(), "device hosts never start a user's sign-in");
    auth.sign_out();
    config.scaffold = None;
    config.allow_dev_auth = true;
    let restarted = Auth::new(config);
    assert!(restarted.device_mode());
    assert_eq!(Engine::initial_workspace_scope(&restarted), WorkspaceScope::Synced);
    assert!(!restarted.pending_scaffold_verification());
    assert!(!restarted.dev_enabled());
    assert_eq!(restarted.state(), AuthState::SignedOut);
    assert!(!std::fs::read_to_string(&path).unwrap().contains("cs1."));
    server.await.unwrap();
}

/// Crew's case keeps crew commands in the retained store; upstream's store
/// has none, so a marker file stands in for that state here.
#[tokio::test]
async fn sandbox_storage_survives_epoch_change_without_reusing_authority() {
    use zeron_sync::scaffold_auth::{DeviceAuthority, DeviceConfig, profile_key};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let other_origin = origin.replace("127.0.0.1", "localhost");
    let grant_id = "0123456789abcdef0123456789abcdef";
    let first = ScaffoldSession {
        access_token: format!("cs1.{grant_id}.current"), resource: origin.clone(), edge_origin: origin.clone(),
        identity: ScaffoldIdentity { user_id:"owner".into(), email:"owner@example.com".into(), name:None, project_scope:"canary".into(), deployment_id:Some("deployment".into()), capabilities:vec!["session.read".into()] },
        device: Some(DeviceAuthority { grant_id:grant_id.into(), expires_at:chrono::Utc::now().timestamp_millis() + 3_600_000, device_id:"device-e8".into(), session_id:"session".into(), deployment_id:"deployment".into(), sandbox_id:"sandbox".into(), lifecycle_epoch:8 }),
    };
    let authority = Arc::new(Mutex::new(first.clone()));
    let server_authority = authority.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let size = stream.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..size]).starts_with("POST /auth/device-token/refresh "));
            let session = lock(&server_authority).clone();
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
    let upload = legacy.join("uploads/retained.txt");
    std::fs::create_dir_all(upload.parent().unwrap()).unwrap();
    std::fs::write(&upload, b"absolute upload path survives").unwrap();
    std::fs::write(legacy.join("retained-state"), b"session state").unwrap();
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
        *lock(&authority) = session.clone();
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
        assert_eq!(profile.org_id(), session.identity.project_scope);
        if field != "epoch" {
            assert!(isolated_roots.insert(profile.store_root().to_path_buf()), "{field}");
        }
        if field == "first" || field == "epoch" {
            assert_eq!(profile.store_root(), legacy);
            assert_eq!(std::fs::read(&upload).unwrap(), b"absolute upload path survives");
            assert_eq!(std::fs::read(profile.store_root().join("retained-state")).unwrap(), b"session state");
        } else {
            assert_ne!(profile.store_root(), legacy, "{field}");
            assert!(!profile.store_root().join("retained-state").exists(), "{field}");
            assert!(!profile.uploads_root().join("retained.txt").exists(), "{field}");
        }
        if field == "first" {
            original_profile = Some(profile);
        } else if field == "epoch" {
            let old = original_profile.as_ref().unwrap();
            assert_ne!(old.scaffold_profile_key(), profile.scaffold_profile_key());
            let mut runtime_config = engine_config(dir.path(), &session.edge_origin, config.scaffold);
            runtime_config.device = config.device;
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

#[test]
fn sandbox_storage_directory_is_the_verified_storage_tuple() {
    use sha2::{Digest, Sha256};
    let data = std::path::Path::new("/isolated/crew-data");
    let binding = serde_json::to_vec(&(
        "scaffold-sandbox-storage-v1", "https://control", "https://edge", "owner", "project",
        Some("deployment"), "deployment", "session", "sandbox",
    )).unwrap();
    let expected = data.join("profiles/scaffold").join(format!("{:x}", Sha256::digest(binding)));
    let actual = zeron_engine::profile::scaffold_profile_directory(data, "https://control", "https://edge", "owner", "project",
        Some("deployment"), "deployment", "session", "sandbox");
    assert_eq!(actual, expected);
    for (owner, session, sandbox) in [("other", "session", "sandbox"), ("owner", "other", "sandbox"), ("owner", "session", "other")] {
        assert_ne!(actual, zeron_engine::profile::scaffold_profile_directory(data, "https://control", "https://edge", owner, "project",
            Some("deployment"), "deployment", session, sandbox));
    }
    // Upstream's profiles carry none of a Scaffold profile's bindings.
    let dir = tempfile::tempdir().unwrap();
    for profile in [EngineProfile::local(dir.path()).unwrap(), EngineProfile::synced(dir.path(), "org", "user")] {
        assert!(!profile.managed_scaffold());
        assert!(profile.scaffold_profile_key().is_none() && profile.device_id_override().is_none());
    }
}

#[cfg(unix)]
mod terminal_login {
    use super::*;
    use std::os::fd::FromRawFd;
    use std::process::Stdio;
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

struct Authority {
    control_status: AtomicU16,
    edge_status: AtomicU16,
    control_calls: AtomicUsize,
    edge_calls: AtomicUsize,
    device_calls: AtomicUsize,
    device_ttl_ms: AtomicUsize,
    fixed_device_expiry: Mutex<Option<i64>>,
    device_bearers: Mutex<Vec<String>>,
    retry_after: Mutex<String>,
    replacement_user: Mutex<&'static str>,
    hold: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    /// Every path asked, in order (no bearer or body).
    paths: Mutex<Vec<String>>,
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
            device_calls: AtomicUsize::new(0), device_ttl_ms: AtomicUsize::new(3_600_000),
            fixed_device_expiry: Mutex::new(None),
            device_bearers: Mutex::new(Vec::new()),
            retry_after: Mutex::new(String::new()), replacement_user: Mutex::new("replacement"),
            hold: AtomicBool::new(false), entered: tokio::sync::Notify::new(), release: tokio::sync::Notify::new(),
            paths: Mutex::new(Vec::new()),
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
                    let mut parts = request.split_whitespace();
                    let method = parts.next().unwrap();
                    let path = parts.next().unwrap();
                    lock(&state.paths).push(path.split('?').next().unwrap_or_default().to_owned());
                    if method == "HEAD" && path == "/" {
                        stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                        return;
                    }
                    let user = if request.contains("Bearer sc_rc_new") {
                        *lock(&state.replacement_user)
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
                            serde_json::json!({"ok":true,"resource":resource,"actor":{"sub":user,"auth":"iap","displayName":"Original User"},"scopes":scopes})
                        }
                        "/auth/session" => {
                            state.edge_calls.fetch_add(1, Ordering::SeqCst);
                            code = state.edge_status.load(Ordering::SeqCst);
                            serde_json::json!({"userId":user,"email":format!("{user}@example.com"),"projectScope":"canary","credential":"scaffold","capabilities":["session.read"]})
                        }
                        "/auth/device-grants/exchange" | "/auth/device-token/refresh" => {
                            assert_eq!(method, "POST");
                            if path.ends_with("/refresh") {
                                let bearer = request.split_whitespace().find(|part| part.starts_with("cs1.")).unwrap();
                                assert!(bearer.starts_with("cs1.0123456789abcdef0123456789abcdef."));
                                lock(&state.device_bearers).push(bearer.to_owned());
                            }
                            let call = state.device_calls.fetch_add(1, Ordering::SeqCst) + 1;
                            code = state.control_status.load(Ordering::SeqCst);
                            if state.hold.swap(false, Ordering::SeqCst) {
                                state.entered.notify_one();
                                state.release.notified().await;
                            }
                            let fixed_expiry = *lock(&state.fixed_device_expiry);
                            let rotation = if fixed_expiry.is_some() { 1 } else { call };
                            let expires_at = fixed_expiry.unwrap_or_else(|| chrono::Utc::now().timestamp_millis() + state.device_ttl_ms.load(Ordering::SeqCst) as i64);
                            serde_json::json!({"accessToken":format!("cs1.0123456789abcdef0123456789abcdef.rotation-{rotation}"),"expiresAt":expires_at,"userId":"original","email":"original@example.com","projectId":"canary","deploymentId":"deployment","sandboxId":"sandbox","targetDeviceId":"device","sessionId":"session","lifecycleEpoch":9,"capabilities":["session.read"]})
                        }
                        "/.well-known/oauth-protected-resource" => serde_json::json!({"resource":resource,"authorization_servers":[resource],"scopes_supported":scopes}),
                        "/.well-known/oauth-authorization-server" => serde_json::json!({"issuer":resource,"authorization_endpoint":format!("{resource}/authorize"),"token_endpoint":format!("{resource}/token"),"registration_endpoint":format!("{resource}/register"),"code_challenge_methods_supported":["S256"]}),
                        "/register" => serde_json::json!({"client_id":"test-client"}),
                        "/token" => serde_json::json!({"access_token":"sc_rc_new","token_type":"Bearer","scope":DEFAULT_OAUTH_SCOPES,"resource":resource}),
                        // Nothing else is this authority's: whatever asks gets nothing.
                        _ => {
                            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                            return;
                        }
                    }.to_string();
                    let retry = lock(&state.retry_after).clone();
                    let header = if retry.is_empty() { String::new() } else { format!("Retry-After: {retry}\r\n") };
                    // Timeout regressions deliberately close the client before release.
                    let _ = stream.write_all(format!("HTTP/1.1 {code} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{header}Connection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                });
            }
        });
        Self { origin, state, task }
    }

    fn scaffold(&self) -> ScaffoldConfig {
        ScaffoldConfig::new(&self.origin, &self.origin, "canary")
    }

    fn auth(&self, dir: &std::path::Path) -> Auth {
        save_session(&dir.join("scaffold-session.json"), &ScaffoldSession {
            access_token: "sc_rc_old".into(), resource: self.origin.clone(), edge_origin: self.origin.clone(), device: None,
            identity: ScaffoldIdentity { user_id: "original".into(), email: "original@example.com".into(), name: None, project_scope: "canary".into(), capabilities: vec!["session.read".into()], deployment_id: None },
        }).unwrap();
        let mut config = AuthConfig::new(&self.origin, dir);
        config.scaffold = Some(self.scaffold());
        Auth::new(config)
    }

    async fn device_auth(&self, dir: &std::path::Path) -> Auth {
        let mut config = AuthConfig::new(&self.origin, dir);
        config.scaffold = Some(self.scaffold());
        config.device = Some(zeron_sync::scaffold_auth::DeviceConfig {
            join_grant: "cg1.0123456789abcdef0123456789abcdef.grant".into(),
            device_id: "device".into(), session_id: "session".into(), deployment_id: "deployment".into(),
            sandbox_id: "sandbox".into(), lifecycle_epoch: 9,
        });
        Auth::detect(config).await
    }

    fn calls(&self) -> (usize, usize) {
        (self.state.control_calls.load(Ordering::SeqCst), self.state.edge_calls.load(Ordering::SeqCst))
    }

    fn device_calls(&self) -> usize {
        self.state.device_calls.load(Ordering::SeqCst)
    }
}

/// zgx's flow, at the engine's RPC surface: signed out and local at start,
/// `SignIn` gives Scaffold's page with a redirect to this engine's loopback
/// on a port of the moment, the browser lands there, and `AuthStatus` says
/// `signedIn` with the project scope as the organization. The next start
/// opens that verified identity's own profile.
#[tokio::test]
async fn the_engine_signs_in_with_scaffold_and_reports_the_project_as_its_organization() {
    let server = ManagedServer::start().await;
    *lock(&server.state.replacement_user) = "user-1";
    let dir = tempfile::tempdir().unwrap();
    let config = engine_config(dir.path(), &server.origin, Some(server.scaffold()));
    let auth = Engine::build_auth(&config).await;
    assert!(auth.scaffold_enabled() && auth.is_authenticated_mode());
    assert_eq!(Engine::initial_workspace_scope(&auth), WorkspaceScope::Local);
    assert!(lock(&server.state.paths).is_empty(), "a clean start asks the authority nothing");
    let rpc = zeron_rpc::memory_client(Arc::new(zeron_engine::rpc::AuthRpc::new(auth.clone())));
    let mut status = rpc.subscribe(methods::AUTH_STATUS, serde_json::json!({})).await.unwrap();
    assert_eq!(status.recv().await.unwrap(), serde_json::json!({"state": "signedOut"}));

    // zgx asks as it does of WorkOS; Scaffold's page has no hints to take.
    let answer = rpc.call(methods::SIGN_IN, serde_json::json!({"screenHint": "sign-in", "fresh": true})).await.unwrap();
    let url = reqwest::Url::parse(answer["url"].as_str().unwrap()).unwrap();
    assert_eq!(url.as_str().split('?').next(), Some(format!("{}/authorize", server.origin).as_str()));
    let params = url.query_pairs().into_owned().collect::<std::collections::HashMap<_, _>>();
    assert_eq!(params["resource"], server.origin);
    assert_eq!(params["code_challenge_method"], "S256");
    let redirect = reqwest::Url::parse(&params["redirect_uri"]).unwrap();
    assert_eq!((redirect.host_str(), redirect.path()), (Some("127.0.0.1"), "/callback"));
    let port = redirect.port().unwrap();
    assert!(port != 27641 && port != 0, "an ephemeral callback port, not WorkOS's 27641");
    // A callback that isn't Scaffold's answer changes nothing.
    let forged = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/callback?state=forged&code=forged"))
        .send().await.unwrap();
    assert_eq!(forged.status().as_u16(), 400);
    assert_eq!(auth.state(), AuthState::SignedOut);

    let mut callback = redirect.clone();
    callback.query_pairs_mut().append_pair("state", &params["state"]).append_pair("code", "browser-code");
    let landed = reqwest::Client::new().get(callback).send().await.unwrap();
    assert!(landed.status().is_success());
    let signed_in = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let item = status.recv().await.unwrap();
            if item["state"] == "signedIn" {
                break item;
            }
        }
    }).await.unwrap();
    assert_eq!(signed_in, serde_json::json!({
        "state": "signedIn",
        "user": {"id": "user-1", "email": "user-1@example.com", "name": "Original User"},
        "orgId": "canary",
    }));
    let saved = dir.path().join("scaffold-session.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let file = std::fs::read_to_string(&saved).unwrap();
    assert!(file.contains("sc_rc_new") && file.contains("canary"));
    assert_eq!(rpc.call(methods::LIST_ORGS, serde_json::json!({})).await.unwrap(), serde_json::json!({"orgs": []}));

    // The next start: the retained credential verified, its own profile.
    drop(rpc);
    let restarted = Engine::build_auth(&config).await;
    assert!(matches!(restarted.state(), AuthState::SignedIn { org_id: Some(org), .. } if org == "canary"));
    let scope = Engine::initial_workspace_scope(&restarted);
    assert_eq!(scope, WorkspaceScope::Synced);
    let profile = Engine::resolve_profile(&config, &restarted, scope).unwrap().unwrap();
    assert!(profile.managed_scaffold());
    assert_eq!((profile.org_id(), profile.user_id()), ("canary", "user-1"));
    assert_eq!(profile.scaffold_profile_key(), restarted.verified_profile_key().as_deref());
    assert!(profile.store_root().starts_with(dir.path().join("profiles/scaffold")));
    // Signing out forgets it, and the start after that is local again.
    restarted.sign_out();
    assert!(!saved.exists());
    assert_eq!(Engine::initial_workspace_scope(&Engine::build_auth(&config).await), WorkspaceScope::Local);
}

/// The `sc_rc_` bearer never leaves the engine, whatever is signed in, and
/// what upstream's account has but Scaffold's doesn't is refused, not tried.
#[tokio::test]
async fn scaffold_mode_refuses_the_access_token_and_upstream_account_calls() {
    let server = ManagedServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = server.auth(dir.path());
    let rpc = zeron_rpc::memory_client(Arc::new(zeron_engine::rpc::AuthRpc::new(auth.clone())));
    let refused = [
        (methods::ACCESS_TOKEN, serde_json::json!({})),
        (methods::STEP_UP, serde_json::json!({})),
        (methods::DELETE_ACCOUNT, serde_json::json!({"confirm": "original@example.com"})),
        (methods::CREATE_ORG, serde_json::json!({"name": "Elsewhere"})),
        (methods::SELECT_ORG, serde_json::json!({"organizationId": "elsewhere"})),
    ];
    let refuses_all = || async {
        for (method, params) in &refused {
            match rpc.call(method, params.clone()).await {
                Err(RpcError::Capability(code)) => assert_eq!(code, zeron_engine::auth::SCAFFOLD_UNSUPPORTED, "{method}"),
                other => panic!("{method}: {other:?}"),
            }
        }
    };
    // Signed out (its retained credential not yet verified)…
    refuses_all().await;
    assert_eq!(server.calls(), (0, 0), "a refusal asks nobody anything");
    // …and signed in.
    assert_eq!(auth.access_token().await.unwrap(), "sc_rc_old");
    assert!(auth.state().is_signed_in());
    refuses_all().await;
    assert!(auth.create_org("Elsewhere").await.is_err() && auth.select_org("elsewhere").await.is_err());
    assert_eq!(rpc.call(methods::LIST_ORGS, serde_json::json!({})).await.unwrap(), serde_json::json!({"orgs": []}));
    assert_eq!(server.calls(), (1, 1));
    assert!(lock(&server.state.paths).iter().all(|path| path.ends_with("/auth/session")), "nothing but verification");
    assert!(auth.state().is_signed_in(), "a refusal signs nobody out");

    // Upstream's modes answer `AccessToken` as they always have.
    let other = tempfile::tempdir().unwrap();
    let mut dev = AuthConfig::new("http://127.0.0.1:9", other.path());
    dev.allow_dev_auth = true;
    dev.dev_user_id = "dev-user".into();
    let dev_rpc = zeron_rpc::memory_client(Arc::new(zeron_engine::rpc::AuthRpc::new(Auth::new(dev))));
    assert_eq!(dev_rpc.call(methods::ACCESS_TOKEN, serde_json::json!({})).await.unwrap()["token"], "dev-user");
}

async fn consumers(auth: &Auth) -> Vec<Result<String, TokenError>> {
    let barrier = Arc::new(tokio::sync::Barrier::new(20));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let auth = auth.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move { barrier.wait().await; TokenSource::token(&auth).await });
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
async fn managed_startup_outage_defers_the_same_synced_profile_until_background_recovery() {
    let server = ManagedServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = engine_config(dir.path(), &server.origin, Some(server.scaffold()));
    let original = server.auth(dir.path());
    original.access_token().await.unwrap();
    let historical = Engine::resolve_profile(&config, &original, WorkspaceScope::Synced).unwrap().unwrap();
    // No harnesses: nothing here may reach beyond the loopback authority.
    let core = EngineCore::assemble_with_profile(historical.clone(), Arc::new(HarnessRegistry::new()), HarnessId::Mock, None).unwrap();
    core.workspace.create_chat("retained-chat", None, Some(&core.device_id), None, None).unwrap();
    core.doc_host.open("retained-chat").unwrap()
        .write_user_message("retained-message", "Existing synced history", 1).unwrap();
    core.shutdown().await;
    drop(core);
    drop(original);
    let credential = std::fs::read(dir.path().join("scaffold-session.json")).unwrap();

    server.state.control_status.store(503, Ordering::SeqCst);
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    assert_eq!(scope, WorkspaceScope::Synced);
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(auth.loaded_scaffold_session());
    assert!(auth.pending_scaffold_verification());
    assert!(auth.verified_scaffold_session().is_none());
    assert!(Engine::resolve_profile(&config, &auth, scope).unwrap().is_none());
    let unverified_tokens = auth.for_profile_tokens();
    assert!(matches!(TokenSource::token(unverified_tokens.as_ref()).await, Err(TokenError::TemporarilyUnavailable(_))),
        "verification outages deny a bearer without claiming the retained credential was revoked");
    assert!(!dir.path().join("local-profile.json").exists());
    assert!(!dir.path().join("profiles/local").exists());
    assert_eq!(std::fs::read(dir.path().join("scaffold-session.json")).unwrap(), credential);

    // Both boot paths wait on this state; only the existing background scheduler
    // retries. Expire its shared failure cooldown before delivering an online hint.
    elapse(Duration::from_secs(6)).await;
    server.state.control_status.store(200, Ordering::SeqCst);
    let mut state = auth.watch_state();
    let refresh_loop = auth.spawn_refresh_loop();
    tokio::task::yield_now().await;
    zeron_sync::wake::notify_online();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state.borrow().is_signed_in() { state.changed().await.unwrap(); }
    }).await.expect("background verification activates the deferred workspace without restart");
    refresh_loop.abort();
    assert!(!auth.pending_scaffold_verification());
    assert_eq!(TokenSource::token(unverified_tokens.as_ref()).await, Err(TokenError::SignedOut),
        "a token source captured without verified identity cannot adopt the recovered profile");
    let recovered = Engine::resolve_profile(&config, &auth, scope).unwrap().unwrap();
    assert_eq!(recovered, historical);
    assert_eq!(Engine::initial_workspace_scope(&auth), scope);
    let core = EngineCore::assemble_with_profile(recovered, Arc::new(HarnessRegistry::new()), HarnessId::Mock, None).unwrap();
    assert!(core.workspace.chat("retained-chat").unwrap().is_some());
    let entries = core.doc_host.open("retained-chat").unwrap().doc().read_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "retained-message");
    core.shutdown().await;
    drop(core);
    assert!(!dir.path().join("local-profile.json").exists());
    assert_eq!(std::fs::read(dir.path().join("scaffold-session.json")).unwrap(), credential);

    // Rejection is not pending recovery and may never turn the captured boundary
    // local. A new signed-out process, unlike this one, has no synced intent.
    server.state.control_status.store(401, Ordering::SeqCst);
    let rejected = Engine::build_auth(&config).await;
    assert_eq!(Engine::initial_workspace_scope(&rejected), WorkspaceScope::Synced);
    assert!(!rejected.pending_scaffold_verification());
    assert!(Engine::resolve_profile(&config, &rejected, WorkspaceScope::Synced).is_err());
    assert!(!dir.path().join("scaffold-session.json").exists());
    let signed_out = Engine::build_auth(&config).await;
    assert_eq!(Engine::initial_workspace_scope(&signed_out), WorkspaceScope::Local);
    assert!(!signed_out.pending_scaffold_verification());
    assert!(Engine::resolve_profile(&config, &signed_out, WorkspaceScope::Local).unwrap().is_some());
}

#[tokio::test]
async fn managed_consumers_idle_past_sixty_seconds_do_not_reverify_with_refresh_loop() {
    let server = ManagedServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = server.auth(dir.path());
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(auth.verified_profile_key().is_none());
    assert!(consumers(&auth).await.iter().all(|result| result.as_deref() == Ok("sc_rc_old")));
    let bound = auth.for_profile_tokens();
    let credential = TokenSource::token(bound.as_ref()).await.unwrap();
    for _ in 0..20 { assert_eq!(TokenSource::token(bound.as_ref()).await.unwrap(), credential); }
    assert_eq!(server.calls(), (1, 1));
    let state_changes = auth.watch_state();
    let token_changes = TokenSource::subscribe(bound.as_ref()).unwrap();
    let identity = auth.state();
    let refresh_loop = auth.spawn_refresh_loop();
    tokio::task::yield_now().await;
    for _ in 0..3 {
        elapse(Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        assert!(consumers(&auth).await.iter().all(|result| result.as_ref() == Ok(&credential)));
        assert_eq!(server.calls(), (1, 1));
        assert_eq!(auth.state(), identity);
        assert!(!state_changes.has_changed().unwrap(), "idle auth must not signal a profile restart");
        assert!(!token_changes.has_changed().unwrap(), "idle auth must not signal a room reconnect");
    }
    refresh_loop.abort();
}

#[tokio::test]
async fn managed_429_shares_retry_after_and_resumes_after_cooldown() {
    let server = ManagedServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = server.auth(dir.path());
    server.state.control_status.store(429, Ordering::SeqCst);
    *lock(&server.state.retry_after) = "30".into();
    let results = consumers(&auth).await;
    assert!(matches!(&results[0], Err(TokenError::TemporarilyUnavailable(_))));
    assert!(results.iter().all(|result| result == &results[0]));
    assert_eq!(server.calls(), (1, 0));
    assert!(dir.path().join("scaffold-session.json").exists());
    assert!(!auth.state().is_signed_in());
    server.state.control_status.store(200, Ordering::SeqCst);
    auth.retry_refresh(); // connectivity hints must not defeat Retry-After
    elapse(Duration::from_secs(20)).await;
    assert_eq!(TokenSource::token(&auth).await, results[0]);
    assert_eq!(server.calls(), (1, 0));
    elapse(Duration::from_secs(11)).await;
    assert!(consumers(&auth).await.iter().all(Result::is_ok));
    assert_eq!(server.calls(), (2, 1));
    assert!(auth.state().is_signed_in());
}

/// Crew's case first verifies, then rejects that lease (the deferred fence)
/// to start a second verification; here the first one is held instead.
#[tokio::test]
async fn managed_signout_fences_inflight_success_and_failure() {
    for status in [200, 429, 503] {
        let server = ManagedServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let auth = server.auth(dir.path());
        server.state.control_status.store(status, Ordering::SeqCst);
        server.state.hold.store(true, Ordering::SeqCst);
        let pending = tokio::spawn({ let auth = auth.clone(); async move { auth.access_token().await } });
        server.state.entered.notified().await;
        auth.sign_out();
        server.state.release.notify_one();
        assert_eq!(pending.await.unwrap(), Err(TokenError::SignedOut), "{status}");
        assert_eq!(auth.access_token().await, Err(TokenError::SignedOut));
        assert_eq!(auth.state(), AuthState::SignedOut);
        assert!(auth.loaded_scaffold_session());
        assert!(!auth.pending_scaffold_verification());
        assert_eq!(Engine::initial_workspace_scope(&auth), WorkspaceScope::Synced);
        assert!(auth.verified_profile_key().is_none());
        assert!(!dir.path().join("scaffold-session.json").exists());
        server.state.control_status.store(200, Ordering::SeqCst);
        replace_credential(&auth).await;
        assert_eq!(auth.access_token().await.unwrap(), "sc_rc_new");
    }
}

/// As Crew's, less the rejection fence: a held verification's late success,
/// rejection or cooldown never lands on the credential that replaced it.
#[tokio::test]
async fn managed_replacement_fences_old_success_rejection_and_cooldown() {
    for status in [200, 401, 503] {
        for user in ["original", "replacement"] {
            let server = ManagedServer::start().await;
            *lock(&server.state.replacement_user) = user;
            let dir = tempfile::tempdir().unwrap();
            let auth = server.auth(dir.path());
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
            assert!(matches!(pending.await, Err(TokenError::TemporarilyUnavailable(_))), "{status} {user}");
            assert_eq!(auth.user_id().as_deref(), Some(user));
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
        if edge_failure { server.state.edge_status.store(403, Ordering::SeqCst); }
        else { server.state.control_status.store(401, Ordering::SeqCst); }
        assert!(consumers(&auth).await.iter().all(|result| *result == Err(TokenError::SignedOut)));
        let calls = server.calls();
        server.state.control_status.store(200, Ordering::SeqCst);
        server.state.edge_status.store(200, Ordering::SeqCst);
        let bound = auth.for_profile_tokens();
        assert_eq!(TokenSource::token(bound.as_ref()).await, Err(TokenError::SignedOut));
        assert_eq!(auth.access_token().await, Err(TokenError::SignedOut));
        assert_eq!(server.calls(), calls, "a rejected credential is never asked about again");
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
        assert_eq!(TokenSource::token(&auth).await, failures[0]);
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
    *lock(&server.state.retry_after) = "600".into();
    assert!(matches!(auth.access_token().await, Err(TokenError::TemporarilyUnavailable(_))));
    server.state.control_status.store(200, Ordering::SeqCst);
    replace_credential(&auth).await;
    assert_eq!(auth.user_id().as_deref(), Some("replacement"));
    assert_eq!(server.calls(), (2, 1));
    elapse(Duration::from_secs(60)).await;
    assert_eq!(auth.access_token().await.unwrap(), "sc_rc_new");
    assert_eq!(server.calls(), (2, 1));
}

#[tokio::test]
async fn device_expiry_slack_coalesces_concurrent_renewal() {
    let server = ManagedServer::start().await;
    server.state.device_ttl_ms.store(32_000, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let auth = server.device_auth(dir.path()).await;
    let initial = auth.access_token().await.unwrap();
    assert_eq!(server.device_calls(), 1, "do not renew before the thirty-second slack");
    let renew_at = auth.device_grant_authority().unwrap().expires_at - 30_000;
    server.state.device_ttl_ms.store(3_600_000, Ordering::SeqCst);
    let remaining = renew_at.saturating_sub(chrono::Utc::now().timestamp_millis()).max(0) as u64;
    tokio::time::sleep(Duration::from_millis(remaining.max(1_000) + 20)).await;
    let leases = consumers(&auth).await;
    let renewed = auth.access_token().await.unwrap();
    assert_ne!(initial, renewed);
    assert!(leases.iter().all(|result| result.as_ref() == Ok(&renewed)));
    assert_eq!(server.device_calls(), 2);
}

/// As Crew's, less the rejection fence's part.
#[tokio::test]
async fn device_expired_token_is_not_served_after_failure_and_retry_after_prevents_amplification() {
    let server = ManagedServer::start().await;
    server.state.device_ttl_ms.store(2_000, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let auth = server.device_auth(dir.path()).await;
    auth.access_token().await.unwrap();
    let expires_at = auth.device_grant_authority().unwrap().expires_at;
    let identity = auth.state();
    let profile = auth.verified_profile_key().unwrap();
    let persisted = std::fs::read(dir.path().join("scaffold-device-session.json")).unwrap();
    server.state.control_status.store(503, Ordering::SeqCst);
    *lock(&server.state.retry_after) = "30".into();
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let failures = consumers(&auth).await;
    assert!(failures.iter().all(|result| matches!(result, Err(TokenError::TemporarilyUnavailable(_)))));
    assert_eq!(server.device_calls(), 2);
    let refresh_loop = auth.spawn_refresh_loop();
    let remaining = expires_at.saturating_sub(chrono::Utc::now().timestamp_millis()).max(0) as u64;
    tokio::time::sleep(Duration::from_millis(remaining + 20)).await;
    assert!(auth.device_grant_authority().is_none(), "wall-clock expiry ends grant authority");
    auth.retry_refresh();
    zeron_sync::wake::notify_online();
    assert!(consumers(&auth).await.iter().all(|result| matches!(result, Err(TokenError::TemporarilyUnavailable(_)))));
    assert_eq!(server.device_calls(), 2, "expired consumers and connectivity hints share Retry-After");
    assert_eq!(auth.state(), identity);
    assert_eq!(auth.verified_profile_key().as_deref(), Some(profile.as_str()));
    assert_eq!(std::fs::read(dir.path().join("scaffold-device-session.json")).unwrap(), persisted);
    server.state.control_status.store(200, Ordering::SeqCst);
    server.state.device_ttl_ms.store(3_600_000, Ordering::SeqCst);
    elapse(Duration::from_secs(6)).await;
    assert!(consumers(&auth).await.iter().all(|result| matches!(result, Err(TokenError::TemporarilyUnavailable(_)))));
    assert_eq!(server.device_calls(), 2, "server Retry-After exceeds the five-second exponential-backoff cap");
    elapse(Duration::from_secs(14)).await;
    assert!(consumers(&auth).await.iter().all(|result| matches!(result, Err(TokenError::TemporarilyUnavailable(_)))));
    assert_eq!(server.device_calls(), 2);
    elapse(Duration::from_secs(11)).await;
    assert!(consumers(&auth).await.iter().all(Result::is_ok));
    assert_eq!(server.device_calls(), 3);
    assert!(auth.device_grant_authority().is_some());
    refresh_loop.abort();
}

#[tokio::test]
async fn device_headerless_failures_share_bounded_backoff() {
    let server = ManagedServer::start().await;
    server.state.device_ttl_ms.store(20_000, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let auth = server.device_auth(dir.path()).await;
    server.state.control_status.store(429, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    for (attempt, delay) in [1, 2, 4, 5, 5].into_iter().enumerate() {
        assert!(consumers(&auth).await.iter().all(|result| matches!(result, Err(TokenError::TemporarilyUnavailable(_)))));
        let calls = attempt + 2;
        assert_eq!(server.device_calls(), calls);
        elapse(Duration::from_millis(delay * 1_000 - 100)).await;
        assert!(consumers(&auth).await.iter().all(|result| matches!(result, Err(TokenError::TemporarilyUnavailable(_)))));
        assert_eq!(server.device_calls(), calls);
        elapse(Duration::from_millis(400)).await; // Cross the deadline including up to 255ms jitter.
    }
    server.state.control_status.store(200, Ordering::SeqCst);
    server.state.device_ttl_ms.store(3_600_000, Ordering::SeqCst);
    assert!(consumers(&auth).await.iter().all(Result::is_ok));
    assert_eq!(server.device_calls(), 7);
}

#[tokio::test]
async fn device_short_unchanged_reply_does_not_spin_foreground_or_background_refresh() {
    let server = ManagedServer::start().await;
    *lock(&server.state.fixed_device_expiry) = Some(chrono::Utc::now().timestamp_millis() + 20_000);
    let dir = tempfile::tempdir().unwrap();
    let auth = server.device_auth(dir.path()).await;
    let initial = auth.access_token().await.unwrap();
    let expiry = auth.device_grant_authority().unwrap().expires_at;
    let refresh_loop = auth.spawn_refresh_loop();
    assert!(consumers(&auth).await.iter().all(|result| result.as_ref() == Ok(&initial)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.device_calls(), 1);
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(consumers(&auth).await.iter().all(|result| result.as_ref() == Ok(&initial)));
    assert_eq!(auth.device_grant_authority().unwrap().expires_at, expiry);
    assert_eq!(server.device_calls(), 2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(consumers(&auth).await.iter().all(Result::is_ok));
    assert_eq!(server.device_calls(), 2, "unchanged near-expiry replies must not trigger an immediate retry loop");
    refresh_loop.abort();
}

/// Crew's case also rejects the earlier lease mid-rotation (the deferred
/// fence); the sign-out half stands as it is.
#[tokio::test]
async fn device_inflight_rotation_never_survives_signout() {
    let server = ManagedServer::start().await;
    server.state.device_ttl_ms.store(20_000, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let auth = server.device_auth(dir.path()).await;
    let initial = auth.access_token().await.unwrap();
    let path = dir.path().join("scaffold-device-session.json");
    server.state.device_ttl_ms.store(3_600_000, Ordering::SeqCst);
    server.state.hold.store(true, Ordering::SeqCst);
    // Reach ordinary near-expiry renewal without invalidating the first lease.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let pending = tokio::spawn({ let auth = auth.clone(); async move { auth.access_token().await } });
    tokio::time::timeout(Duration::from_secs(3), server.state.entered.notified()).await.unwrap();
    assert_eq!(lock(&server.state.device_bearers).as_slice(), std::slice::from_ref(&initial));
    auth.sign_out();
    server.state.release.notify_one();
    assert!(matches!(pending.await.unwrap(), Err(TokenError::SignedOut)));
    assert!(matches!(auth.access_token().await, Err(TokenError::SignedOut)));
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(auth.device_grant_authority().is_none());
    assert!(!std::fs::read_to_string(&path).unwrap().contains("cs1."), "late rotation must not overwrite the signout tombstone");
    assert_eq!(server.device_calls(), 2);
}
