//! Official Antigravity login import. Native OAuth owns consent and PKCE; Crew
//! only refreshes the resulting grant to identify it before encrypted import.
use super::*;
use crate::scaffold::{AgentAccountGoogleCredential, AgentAccountOAuthCredential};
use futures::StreamExt;
use serde_json::json;

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v3/userinfo";
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_incomplete_or_redirected_native_credentials_before_network() {
        for blob in [
            json!({"refresh_token":"r"}),
            json!({"client_id":"c","client_secret":"s","refresh_token":"r","token_uri":"https://evil.example/token","project_id":"p","scopes":[]}),
            json!({"client_id":"c","client_secret":"s","refresh_token":"r","token_uri":TOKEN_URL,"project_id":"","scopes":[]}),
        ] {
            assert!(NativeCredential::parse(blob).is_err());
        }
    }
}

#[derive(Deserialize)]
struct NativeCredential {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_uri: String,
    scopes: Vec<String>,
    project_id: String,
}

impl NativeCredential {
    fn parse(value: serde_json::Value) -> Result<Self, EngineError> {
        let credential: Self = serde_json::from_value(value).map_err(|_| failed("Official Antigravity login is incomplete"))?;
        if credential.token_uri != TOKEN_URL || [&credential.client_id, &credential.client_secret, &credential.refresh_token, &credential.project_id]
            .iter().any(|value| value.trim().is_empty() || value.len() > 16_384) {
            return Err(failed("Official Antigravity credential metadata is invalid"));
        }
        Ok(credential)
    }
}

fn failed(message: &str) -> EngineError { EngineError::Other(message.into()) }

/// Capture the installed official client's own attribution header. The private
/// catalog returns metadata only and never contacts a model provider.
pub(super) async fn gateway_user_agent(harness: &zeron_harness::AcpHarness) -> Result<String, EngineError> {
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::{Request, Response, body::Incoming, service::service_fn};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let token = new_id();
    let expected = format!("Bearer {token}");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(tx)));
    let serving = tokio::spawn(async move {
        // A bounded number of metadata connections; no bodies are read.
        for _ in 0..8 {
            let Ok((stream, peer)) = listener.accept().await else { break };
            if !peer.ip().is_loopback() { continue; }
            let sender = sender.clone();
            let expected = expected.clone();
            let service = service_fn(move |request: Request<Incoming>| {
                let authorized = request.method() == hyper::Method::GET
                    && matches!(request.uri().path(), "/v1beta/models" | "/v1/models")
                    && request.headers().get(hyper::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok()) == Some(expected.as_str());
                if authorized {
                    if let Some(value) = request.headers().get(hyper::header::USER_AGENT)
                        .and_then(|value| value.to_str().ok())
                        .filter(|value| !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)) {
                        if let Some(sender) = lock(&sender).take() { let _ = sender.send(value.to_owned()); }
                    }
                }
                let body = if authorized {
                    crate::inference_relay::antigravity_model_catalog("gemini-3.8-flash-high")
                } else { json!({"error":"invalid_metadata_request"}) };
                async move { Ok::<_, Infallible>(Response::builder().status(if authorized { 200 } else { 403 })
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(body.to_string()))).expect("static metadata response")) }
            });
            let _ = tokio::time::timeout(Duration::from_secs(10), hyper::server::conn::http1::Builder::new()
                .keep_alive(false).max_buf_size(16 * 1024).serve_connection(TokioIo::new(stream), service)).await;
        }
    });
    // Dropping a cancelled sign-in must not leave a metadata listener behind.
    let abort = serving.abort_handle();
    struct AbortOnDrop(tokio::task::AbortHandle);
    impl Drop for AbortOnDrop { fn drop(&mut self) { self.0.abort(); } }
    let _guard = AbortOnDrop(abort);
    let context = zeron_harness::RunContext {
        session_id: "antigravity-login-metadata".into(), ipc_port: 0, supervision_token: None, fork_from: None,
        inference: Some(zeron_harness::InferenceRoute { provider: "antigravity".into(),
            model: "gemini-3.8-flash-high".into(), base_url: format!("http://{address}"), token }),
    };
    harness.probe_managed_gateway(&context).await.map_err(|_| failed("Official Antigravity gateway metadata is unavailable"))?;
    tokio::time::timeout(Duration::from_secs(1), rx).await.ok().and_then(Result::ok)
        .ok_or_else(|| failed("Official Antigravity did not emit its gateway identity"))
}

async fn bounded_json(request: reqwest::RequestBuilder) -> Result<serde_json::Value, EngineError> {
    let response = request.timeout(Duration::from_secs(30)).send().await
        .map_err(|_| failed("Google account verification could not connect"))?;
    if !response.status().is_success() {
        return Err(failed("Google rejected account verification; sign in again"));
    }
    if response.content_length().is_some_and(|size| size > MAX_CREDENTIAL_BYTES) {
        return Err(failed("Google account response exceeded its size limit"));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| failed("Google account response was interrupted"))?;
        if bytes.len() + chunk.len() > MAX_CREDENTIAL_BYTES as usize { return Err(failed("Google account response exceeded its size limit")); }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| failed("Google returned an invalid account response"))
}

pub(super) async fn read_slot(home: &Path, user_agent: String) -> Result<Slot, EngineError> {
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
        .build().map_err(|_| failed("Could not initialize Google account verification"))?;
    let path = home.join("antigravity-acp/acp_token.json");
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.len() > MAX_CREDENTIAL_BYTES { return Err(failed("Antigravity credential file is invalid")); }
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(MAX_CREDENTIAL_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CREDENTIAL_BYTES as usize { return Err(failed("Antigravity credential file exceeded its size limit")); }
    let credential = NativeCredential::parse(serde_json::from_slice(&bytes)
        .map_err(|_| failed("Antigravity credential file is invalid"))?)?;
    let token = bounded_json(http.post(TOKEN_URL).form(&[
        ("grant_type", "refresh_token"), ("client_id", &credential.client_id),
        ("client_secret", &credential.client_secret), ("refresh_token", &credential.refresh_token),
    ])).await?;
    let access = str_field(&token, "access_token").ok_or_else(|| failed("Google returned no access token"))?;
    let seconds = token.get("expires_in").and_then(serde_json::Value::as_i64)
        .filter(|seconds| *seconds > 0 && *seconds <= 86400).ok_or_else(|| failed("Google returned invalid token expiry"))?;
    let identity = bounded_json(http.get(USERINFO_URL).bearer_auth(&access)).await?;
    let subject = str_field(&identity, "sub").ok_or_else(|| failed("Google account identity is missing"))?;
    let email = str_field(&identity, "email").ok_or_else(|| failed("Google account email is missing"))?;
    if identity.get("email_verified").and_then(serde_json::Value::as_bool) != Some(true) { return Err(failed("Google account email is not verified")); }
    let imported = AgentAccountOAuthCredential {
        access_token: access,
        refresh_token: str_field(&token, "refresh_token").unwrap_or(credential.refresh_token),
        expires_at: (Utc::now() + chrono::TimeDelta::seconds(seconds)).to_rfc3339(),
        scopes: credential.scopes,
        google: Some(AgentAccountGoogleCredential { client_id: credential.client_id, client_secret: credential.client_secret,
            project_id: credential.project_id, endpoint: "https://daily-cloudcode-pa.googleapis.com".into(),
            user_agent: Some(user_agent) }),
    };
    Ok(Slot {
        id: slot_id_for(HarnessId::Antigravity, &subject), harness: HarnessId::Antigravity, account_key: subject,
        profile: SlotProfile { email, display_name: str_field(&identity, "name"), organization: None, plan: None, auth_kind: AgentAuthKind::Oauth },
        credentials: serde_json::to_value(imported).map_err(|_| failed("Could not prepare Google credential import"))?,
        claude_config: None, saved_at: now_ms(), created_at: None, store_key: None,
    })
}
