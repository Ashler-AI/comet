//! Scaffold OAuth DCR + PKCE authentication tests against a local control plane.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use comet_engine::{Auth, AuthConfig, AuthState};

#[derive(Default)]
struct Requests {
    paths: Mutex<Vec<String>>,
    token_body: Mutex<String>,
}

struct StubControlPlane {
    origin: String,
    requests: Arc<Requests>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for StubControlPlane {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl StubControlPlane {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let requests = Arc::new(Requests::default());
        let state = requests.clone();
        let server_origin = origin.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(handle(stream, server_origin.clone(), state.clone()));
            }
        });
        Self {
            origin,
            requests,
            task,
        }
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<(String, String, String)> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = headers.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or_default();
    let mut body = bytes[header_end..].to_vec();
    while body.len() < length {
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..count]);
    }
    Some((method, target, String::from_utf8_lossy(&body).into_owned()))
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: serde_json::Value) {
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("write response");
}

async fn handle(mut stream: tokio::net::TcpStream, origin: String, state: Arc<Requests>) {
    let Some((method, target, body)) = read_request(&mut stream).await else {
        return;
    };
    let path = target.split('?').next().unwrap_or_default().to_string();
    state.paths.lock().expect("paths").push(path.clone());
    match (method.as_str(), path.as_str()) {
        ("GET", "/health") => respond(
            &mut stream,
            "200 OK",
            serde_json::json!({"ok": true, "auth": "scaffold"}),
        )
        .await,
        ("GET", "/.well-known/oauth-protected-resource") => respond(
            &mut stream,
            "200 OK",
            serde_json::json!({
                "resource": origin,
                "authorization_servers": [origin],
                "scopes_supported": ["remote_code:create", "remote_code:read", "remote_code:write", "remote_code:exec", "remote_code:lifecycle"]
            }),
        )
        .await,
        ("GET", "/.well-known/oauth-authorization-server") => respond(
            &mut stream,
            "200 OK",
            serde_json::json!({
                "issuer": origin,
                "authorization_endpoint": format!("{origin}/api/oauth/authorize"),
                "token_endpoint": format!("{origin}/api/oauth/token"),
                "registration_endpoint": format!("{origin}/api/oauth/register"),
                "code_challenge_methods_supported": ["S256"]
            }),
        )
        .await,
        ("POST", "/api/oauth/register") => respond(
            &mut stream,
            "201 Created",
            serde_json::json!({"client_id": "rcoac_test"}),
        )
        .await,
        ("POST", "/api/oauth/token") => {
            *state.token_body.lock().expect("token body") = body;
            respond(
                &mut stream,
                "200 OK",
                serde_json::json!({
                    "access_token": "sc_rc_test_bearer",
                    "token_type": "Bearer",
                    "scope": "remote_code:create remote_code:read remote_code:write remote_code:exec remote_code:lifecycle",
                    "resource": origin
                }),
            )
            .await;
        }
        ("GET", "/api/code-sandboxes/auth/session") => respond(
            &mut stream,
            "200 OK",
            serde_json::json!({
                "ok": true,
                "resource": origin,
                "actor": {"sub": "developer@ashler.ai", "auth": "iap", "displayName": "Developer"},
                "scopes": ["remote_code:create", "remote_code:read", "remote_code:write", "remote_code:exec", "remote_code:lifecycle"]
            }),
        )
        .await,
        _ => respond(&mut stream, "404 Not Found", serde_json::json!({"error": "not_found"})).await,
    }
}

fn config(origin: &str, data_dir: &std::path::Path) -> AuthConfig {
    let mut config = AuthConfig::new(origin, data_dir);
    config.scaffold_url = Some(origin.to_string());
    config.project_scope = "ashler-staging".into();
    config.callback_port = None;
    config
}

#[tokio::test]
async fn dcr_pkce_exchange_uses_remote_code_scopes_and_persists_internal_capabilities() {
    let stub = StubControlPlane::start().await;
    let directory = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(config(&stub.origin, directory.path()));

    let authorize = auth.start_headless_sign_in().await.expect("authorize URL");
    let parsed = reqwest::Url::parse(&authorize).expect("authorize URL");
    assert_eq!(parsed.path(), "/api/oauth/authorize");
    assert_eq!(
        parsed
            .query_pairs()
            .find(|(key, _)| key == "code_challenge_method")
            .map(|(_, value)| value.into_owned())
            .as_deref(),
        Some("S256")
    );
    assert_eq!(
        parsed
            .query_pairs()
            .find(|(key, _)| key == "scope")
            .map(|(_, value)| value.into_owned())
            .as_deref(),
        Some(
            "remote_code:create remote_code:read remote_code:write remote_code:exec remote_code:lifecycle"
        )
    );
    let state = parsed
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("state");
    auth.complete_sign_in(&format!("{state}.one_time_code"))
        .await
        .expect("complete sign in");

    assert_eq!(
        auth.access_token().await.as_deref(),
        Some("sc_rc_test_bearer")
    );
    assert!(matches!(
        auth.state(),
        AuthState::SignedIn { user, project_scope }
            if user.id == "developer@ashler.ai" && project_scope == "ashler-staging"
    ));
    assert_eq!(
        auth.capabilities().join(" "),
        "session.read session.chat session.control session.annotate session.invite session.files session.environment"
    );
    let token_body = stub.requests.token_body.lock().expect("token body").clone();
    assert!(token_body.contains("code_verifier="));
    assert!(token_body.contains("resource="));

    let reloaded = Auth::new(config(&stub.origin, directory.path()));
    assert_eq!(
        reloaded.access_token().await.as_deref(),
        Some("sc_rc_test_bearer")
    );
    assert_eq!(
        reloaded.capabilities().join(" "),
        "session.read session.chat session.control session.annotate session.invite session.files session.environment"
    );
}

#[tokio::test]
async fn explicit_dev_mode_never_contacts_scaffold() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut config = AuthConfig::new("http://127.0.0.1:9", directory.path());
    config.dev_user_id = "local-developer".into();
    config.project_scope = "ashler-local".into();
    config.internal_capabilities = "session.read session.files".into();
    let auth = Auth::new(config);
    assert!(!auth.oauth_enabled());
    assert_eq!(
        auth.access_token().await.as_deref(),
        Some("local-developer")
    );
    assert!(
        matches!(auth.state(), AuthState::SignedIn { project_scope, .. } if project_scope == "ashler-local")
    );
    assert_eq!(auth.capabilities().join(" "), "session.read session.files");
}
