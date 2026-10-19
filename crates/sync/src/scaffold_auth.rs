//! Resource-bound Scaffold OAuth shared by desktop and the engine-free mobile client.
//! User bearers are permanent/revocable; device sessions rotate; join grants never persist.
use std::{collections::HashMap, path::Path, sync::Mutex, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use reqwest::Url;

const TTL: Duration = Duration::from_secs(900);
pub const DEFAULT_OAUTH_SCOPES: &str = "remote_code:create remote_code:read remote_code:write remote_code:exec remote_code:lifecycle";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Scaffold authorization rejected or expired")]
    Rejected,
    #[error("{0}")]
    Unavailable(String),
}

#[derive(Clone, Debug)]
pub struct ScaffoldConfig {
    pub control_origin: String,
    pub edge_origin: String,
    pub project_scope: String,
    pub oauth_scopes: String,
    pub deployment_id: Option<String>,
}
impl ScaffoldConfig {
    pub fn new(control_origin: impl Into<String>, edge_origin: impl Into<String>, project_scope: impl Into<String>) -> Self {
        Self { control_origin: control_origin.into(), edge_origin: edge_origin.into(), project_scope: project_scope.into(), oauth_scopes: DEFAULT_OAUTH_SCOPES.into(), deployment_id: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaffoldIdentity {
    pub user_id: String,
    pub email: String,
    pub name: Option<String>,
    pub project_scope: String,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub deployment_id: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaffoldSession {
    pub access_token: String,
    pub identity: ScaffoldIdentity,
    pub resource: String,
    pub edge_origin: String,
    #[serde(default)]
    pub device: Option<DeviceAuthority>,
}
impl std::fmt::Debug for ScaffoldSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScaffoldSession").field("identity", &self.identity).field("resource", &self.resource).field("edge_origin", &self.edge_origin).field("device", &self.device).field("access_token", &"<redacted>").finish()
    }
}
#[derive(Clone)]
pub struct DeviceConfig {
    pub join_grant: String,
    pub device_id: String,
    pub session_id: String,
    pub deployment_id: String,
    pub sandbox_id: String,
    pub lifecycle_epoch: u64,
}
impl std::fmt::Debug for DeviceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceConfig").field("join_grant", &"<redacted>").field("device_id", &self.device_id).field("session_id", &self.session_id).field("deployment_id", &self.deployment_id).field("sandbox_id", &self.sandbox_id).field("lifecycle_epoch", &self.lifecycle_epoch).finish()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAuthority {
    pub grant_id: String,
    pub expires_at: i64,
    pub device_id: String,
    pub session_id: String,
    pub deployment_id: String,
    pub sandbox_id: String,
    pub lifecycle_epoch: u64,
}
struct Pending {
    created: Instant,
    generation: u64,
    client_id: String,
    redirect_uri: String,
    verifier: String,
    token_endpoint: String,
    resource: String,
}
#[derive(Default)]
struct SignIns { generation: u64, pending: HashMap<String, Pending> }
pub struct ScaffoldAuth {
    config: ScaffoldConfig,
    http: reqwest::Client,
    sign_ins: Mutex<SignIns>,
}
fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> { value.lock().unwrap_or_else(std::sync::PoisonError::into_inner) }
fn unavailable(message: &str) -> AuthError { AuthError::Unavailable(message.into()) }
fn now_ms() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64 }

/// Only exact TLS origins (or explicit loopback HTTP) are authority configuration.
pub fn normalized_origin(value: &str) -> Result<String, AuthError> {
    let url = Url::parse(value).map_err(|_| AuthError::Rejected)?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]" | "::1"));
    if (url.scheme() != "https" && !(local && url.scheme() == "http")) || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() || url.path() != "/" {
        return Err(AuthError::Rejected);
    }
    Ok(url.origin().ascii_serialization())
}
fn same_origin(base: &str, candidate: &str) -> Result<(), AuthError> {
    let url = Url::parse(candidate).map_err(|_| AuthError::Rejected)?;
    if url.origin().ascii_serialization() != base || !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() { return Err(AuthError::Rejected); }
    Ok(())
}
fn has_scopes(required: &str, actual: &[String]) -> bool { required.split_whitespace().all(|scope| actual.iter().any(|value| value == scope)) }
fn challenge(verifier: &str) -> String { URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) }

impl ScaffoldAuth {
    pub fn new(mut config: ScaffoldConfig) -> Result<Self, AuthError> {
        config.control_origin = normalized_origin(&config.control_origin)?;
        config.edge_origin = normalized_origin(&config.edge_origin)?;
        if config.project_scope.trim().is_empty() || config.oauth_scopes.trim().is_empty() { return Err(AuthError::Rejected); }
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).redirect(reqwest::redirect::Policy::none()).build().map_err(|_| unavailable("could not initialize Scaffold HTTP client"))?;
        Ok(Self { config, http, sign_ins: Mutex::new(SignIns::default()) })
    }
    async fn json<T: serde::de::DeserializeOwned>(&self, stage: &str, request: reqwest::RequestBuilder) -> Result<T, AuthError> {
        // Only fixed operation names and error categories cross this boundary;
        // request URLs, bearer headers and authority response bodies stay private.
        let response = request.send().await.map_err(|error| {
            let cause = if error.is_timeout() { "request_timeout" } else if error.is_connect() { "connect_failure" } else { "send_failure" };
            unavailable(&format!("Scaffold {stage}: {cause}"))
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN { return Err(AuthError::Rejected); }
        if !status.is_success() { return Err(unavailable(&format!("Scaffold {stage}: HTTP {}", status.as_u16()))); }
        response.json().await.map_err(|error| unavailable(&format!("Scaffold {stage}: {}", if error.is_timeout() { "response_timeout" } else if error.is_decode() { "invalid_response" } else { "response_body_failure" })))
    }
    pub fn cancel_sign_in(&self) {
        let mut flow = lock(&self.sign_ins);
        flow.generation = flow.generation.wrapping_add(1);
        flow.pending.clear();
    }
    pub async fn begin_sign_in(&self, redirect_uri: &str) -> Result<String, AuthError> {
        let redirect = Url::parse(redirect_uri).map_err(|_| AuthError::Rejected)?;
        if !redirect.username().is_empty() || redirect.password().is_some() || redirect.query().is_some() || redirect.fragment().is_some() { return Err(AuthError::Rejected); }
        // Public-client redirects may be a native app scheme or an exact loopback URL.
        if matches!(redirect.scheme(), "http" | "https") && !(redirect.scheme() == "http" && matches!(redirect.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))) { return Err(AuthError::Rejected); }
        let generation = lock(&self.sign_ins).generation;
        #[derive(Deserialize)] struct Resource { resource: String, authorization_servers: Vec<String>, scopes_supported: Vec<String> }
        #[derive(Deserialize)] struct Server { issuer: String, authorization_endpoint: String, token_endpoint: String, registration_endpoint: String, code_challenge_methods_supported: Vec<String> }
        #[derive(Deserialize)] struct Registration { client_id: String }
        let base = &self.config.control_origin;
        let resource: Resource = self.json("resource discovery", self.http.get(format!("{base}/.well-known/oauth-protected-resource"))).await?;
        // The configured origin is the resource, not any same-origin arbitrary path.
        if normalized_origin(&resource.resource)? != *base || !has_scopes(&self.config.oauth_scopes, &resource.scopes_supported) { return Err(AuthError::Rejected); }
        let issuer = resource.authorization_servers.first().ok_or(AuthError::Rejected)?;
        if normalized_origin(issuer)? != *base { return Err(AuthError::Rejected); }
        let server: Server = self.json("authorization discovery", self.http.get(format!("{base}/.well-known/oauth-authorization-server"))).await?;
        if normalized_origin(&server.issuer)? != *base || !server.code_challenge_methods_supported.iter().any(|value| value == "S256") { return Err(AuthError::Rejected); }
        for endpoint in [&server.authorization_endpoint, &server.token_endpoint, &server.registration_endpoint] { same_origin(base, endpoint)?; }
        let registration: Registration = self.json("client registration", self.http.post(&server.registration_endpoint).json(&serde_json::json!({"redirect_uris":[redirect_uri],"token_endpoint_auth_method":"none","grant_types":["authorization_code"],"response_types":["code"]}))).await?;
        if registration.client_id.trim().is_empty() { return Err(AuthError::Rejected); }
        let verifier = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let state = uuid::Uuid::new_v4().to_string();
        let mut authorize = Url::parse(&server.authorization_endpoint).map_err(|_| AuthError::Rejected)?;
        authorize.query_pairs_mut().append_pair("response_type", "code").append_pair("client_id", &registration.client_id).append_pair("redirect_uri", redirect_uri).append_pair("state", &state).append_pair("code_challenge", &challenge(&verifier)).append_pair("code_challenge_method", "S256").append_pair("resource", &resource.resource).append_pair("scope", &self.config.oauth_scopes);
        let mut flow = lock(&self.sign_ins);
        if flow.generation != generation { return Err(AuthError::Rejected); }
        flow.pending.retain(|_, pending| pending.created.elapsed() < TTL);
        flow.pending.insert(state, Pending { created: Instant::now(), generation, client_id: registration.client_id, redirect_uri: redirect_uri.into(), verifier, token_endpoint: server.token_endpoint, resource: resource.resource });
        Ok(authorize.into())
    }
    pub async fn complete_sign_in(&self, callback: &str) -> Result<ScaffoldSession, AuthError> {
        let (state, code, callback_url) = parse_callback(callback)?;
        let pending = {
            let mut flow = lock(&self.sign_ins);
            let pending = flow.pending.remove(&state).ok_or(AuthError::Rejected)?;
            if pending.created.elapsed() >= TTL || pending.generation != flow.generation { return Err(AuthError::Rejected); }
            pending
        };
        if let Some(mut url) = callback_url {
            url.set_query(None);
            if url.as_str() != pending.redirect_uri { return Err(AuthError::Rejected); }
        }
        #[derive(Deserialize)] struct Tokens { access_token: String, token_type: String, scope: String, resource: String }
        let tokens: Tokens = self.json("token exchange", self.http.post(&pending.token_endpoint).form(&[("grant_type", "authorization_code"), ("code", &code), ("client_id", &pending.client_id), ("redirect_uri", &pending.redirect_uri), ("code_verifier", &pending.verifier), ("resource", &pending.resource)])).await?;
        if !tokens.token_type.eq_ignore_ascii_case("Bearer") || !tokens.access_token.starts_with("sc_rc_") || tokens.resource != pending.resource || !has_scopes(&self.config.oauth_scopes, &tokens.scope.split_whitespace().map(str::to_owned).collect::<Vec<_>>()) { return Err(AuthError::Rejected); }
        let session = self.verify_token(&tokens.access_token).await?;
        if lock(&self.sign_ins).generation != pending.generation { return Err(AuthError::Rejected); }
        Ok(session)
    }
    async fn verify_token(&self, token: &str) -> Result<ScaffoldSession, AuthError> {
        if !token.starts_with("sc_rc_") { return Err(AuthError::Rejected); }
        #[derive(Deserialize)] #[serde(rename_all="camelCase")] struct Actor { sub: String, auth: String, display_name: Option<String> }
        #[derive(Deserialize)] struct Session { ok: bool, resource: String, actor: Actor, scopes: Vec<String> }
        let resource = &self.config.control_origin;
        let verified: Session = self.json("control session verification", self.http.get(format!("{resource}/api/code-sandboxes/auth/session")).bearer_auth(token)).await?;
        if !verified.ok || normalized_origin(&verified.resource)? != *resource || verified.actor.auth != "iap" || verified.actor.sub.trim().is_empty() || !has_scopes(&self.config.oauth_scopes, &verified.scopes) { return Err(AuthError::Rejected); }
        #[derive(Deserialize)] #[serde(rename_all="camelCase")] struct EdgeIdentity { user_id: String, email: String, project_scope: String, capabilities: Vec<String>, credential: String, deployment_id: Option<String> }
        let edge: EdgeIdentity = self.json("Edge session verification", self.http.get(format!("{}/auth/session", self.config.edge_origin)).bearer_auth(token)).await?;
        if edge.credential != "scaffold" || edge.user_id != verified.actor.sub || edge.project_scope != self.config.project_scope || self.config.deployment_id.as_ref().is_some_and(|expected| Some(expected) != edge.deployment_id.as_ref()) { return Err(AuthError::Rejected); }
        Ok(ScaffoldSession { access_token: token.into(), identity: ScaffoldIdentity { user_id: edge.user_id, email: edge.email, name: verified.actor.display_name, project_scope: edge.project_scope, capabilities: edge.capabilities, deployment_id: edge.deployment_id }, resource: resource.clone(), edge_origin: self.config.edge_origin.clone(), device: None })
    }
    pub async fn verify(&self, session: &ScaffoldSession) -> Result<ScaffoldSession, AuthError> {
        if session.resource != self.config.control_origin || session.edge_origin != self.config.edge_origin || session.identity.project_scope != self.config.project_scope || session.device.is_some() { return Err(AuthError::Rejected); }
        let verified = self.verify_token(&session.access_token).await?;
        if verified.identity.user_id != session.identity.user_id || verified.identity.deployment_id != session.identity.deployment_id { return Err(AuthError::Rejected); }
        Ok(verified)
    }
    pub async fn exchange_device(&self, config: &DeviceConfig) -> Result<ScaffoldSession, AuthError> {
        let grant_id = grant_token_id(&config.join_grant, "cg1").ok_or(AuthError::Rejected)?;
        let reply: DeviceReply = self.json("device grant exchange", self.http.post(format!("{}/auth/device-grants/exchange", self.config.edge_origin)).json(&serde_json::json!({"grant":config.join_grant}))).await?;
        self.device_session(reply, config, grant_id)
    }
    pub async fn refresh_device(&self, current: &ScaffoldSession, config: &DeviceConfig) -> Result<ScaffoldSession, AuthError> {
        if current.resource != self.config.control_origin || current.edge_origin != self.config.edge_origin || current.identity.project_scope != self.config.project_scope { return Err(AuthError::Rejected); }
        let authority = current.device.as_ref().ok_or(AuthError::Rejected)?;
        if authority.device_id != config.device_id || authority.session_id != config.session_id
            || authority.deployment_id != config.deployment_id || authority.sandbox_id != config.sandbox_id
            || authority.lifecycle_epoch != config.lifecycle_epoch
        {
            return Err(AuthError::Rejected);
        }
        let reply: DeviceReply = self.json("device token refresh", self.http.post(format!("{}/auth/device-token/refresh", self.config.edge_origin)).bearer_auth(&current.access_token)).await?;
        let next = self.device_session(reply, config, &authority.grant_id)?;
        if next.identity != current.identity { return Err(AuthError::Rejected); }
        Ok(next)
    }
    fn device_session(&self, reply: DeviceReply, config: &DeviceConfig, grant_id: &str) -> Result<ScaffoldSession, AuthError> {
        if self.config.deployment_id.as_ref().is_some_and(|expected| expected != &reply.deployment_id) { return Err(AuthError::Rejected); }
        if grant_token_id(&reply.access_token, "cs1") != Some(grant_id) || reply.expires_at <= now_ms() || reply.user_id.trim().is_empty() || reply.project_id != self.config.project_scope || reply.target_device_id != config.device_id || reply.session_id != config.session_id || reply.deployment_id != config.deployment_id || reply.sandbox_id != config.sandbox_id || reply.lifecycle_epoch != config.lifecycle_epoch || [config.device_id.as_str(), config.session_id.as_str(), config.deployment_id.as_str(), config.sandbox_id.as_str()].iter().any(|value| value.is_empty()) { return Err(AuthError::Rejected); }
        Ok(ScaffoldSession { access_token: reply.access_token, resource: self.config.control_origin.clone(), edge_origin: self.config.edge_origin.clone(), identity: ScaffoldIdentity { user_id: reply.user_id, email: reply.email, name: None, project_scope: reply.project_id, capabilities: reply.capabilities, deployment_id: Some(reply.deployment_id.clone()) }, device: Some(DeviceAuthority { grant_id: grant_id.into(), expires_at: reply.expires_at, device_id: reply.target_device_id, session_id: reply.session_id, deployment_id: reply.deployment_id, sandbox_id: reply.sandbox_id, lifecycle_epoch: reply.lifecycle_epoch }) })
    }
}
#[derive(Deserialize)] #[serde(rename_all="camelCase")]
struct DeviceReply { access_token: String, expires_at: i64, user_id: String, email: String, project_id: String, deployment_id: String, sandbox_id: String, target_device_id: String, session_id: String, lifecycle_epoch: u64, capabilities: Vec<String> }
fn grant_token_id<'a>(token: &'a str, prefix: &str) -> Option<&'a str> {
    let mut parts = token.split('.');
    if parts.next()? != prefix { return None; }
    let id = parts.next()?;
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) || parts.next()?.is_empty() || parts.next().is_some() { return None; }
    Some(id)
}
fn parse_callback(callback: &str) -> Result<(String, String, Option<Url>), AuthError> {
    let callback = callback.trim();
    if callback.contains("://") {
        let url = Url::parse(callback).map_err(|_| AuthError::Rejected)?;
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() { return Err(AuthError::Rejected); }
        let one = |name: &str| -> Result<String, AuthError> {
            let mut values = url.query_pairs().filter(|(key, _)| key == name).map(|(_, value)| value.into_owned());
            let value = values.next().filter(|value| !value.is_empty()).ok_or(AuthError::Rejected)?;
            if values.next().is_some() { return Err(AuthError::Rejected); }
            Ok(value)
        };
        Ok((one("state")?, one("code")?, Some(url)))
    } else {
        let (state, code) = callback.split_once('.').filter(|(state, code)| !state.is_empty() && !code.is_empty()).ok_or(AuthError::Rejected)?;
        Ok((state.into(), code.into(), None))
    }
}
/// Collision-resistant identity boundary; never uses lossy path sanitization.
pub fn profile_key(session: &ScaffoldSession) -> String {
    let binding = serde_json::to_vec(&(&session.resource, &session.edge_origin, &session.identity.project_scope, &session.identity.deployment_id, &session.identity.user_id, session.device.as_ref().map(|device| (&device.deployment_id, &device.session_id, &device.device_id, &device.sandbox_id)))).expect("string tuple serializes");
    format!("{:x}", Sha256::digest(binding))
}
pub fn load_session(path: &Path) -> Result<Option<ScaffoldSession>, AuthError> {
    let bytes = match std::fs::read(path) { Ok(bytes) => bytes, Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(_) => return Err(unavailable("could not read Scaffold credential")) };
    let session: ScaffoldSession = serde_json::from_slice(&bytes).map_err(|_| AuthError::Rejected)?;
    valid_persisted_credential(&session)?;
    Ok(Some(session))
}
pub fn save_session(path: &Path, session: &ScaffoldSession) -> Result<(), AuthError> {
    use std::io::Write;
    valid_persisted_credential(session)?;
    let bytes = serde_json::to_vec(session).map_err(|_| unavailable("could not serialize Scaffold credential"))?;
    let parent = path.parent().ok_or(AuthError::Rejected)?;
    std::fs::create_dir_all(parent).map_err(|_| unavailable("could not create credential directory"))?;
    let temp = parent.join(format!(".scaffold-session-{}", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() { let _ = std::fs::remove_file(temp); }
    result.map_err(|_| unavailable("could not persist Scaffold credential"))
}
fn valid_persisted_credential(session: &ScaffoldSession) -> Result<(), AuthError> {
    match &session.device {
        Some(device) if grant_token_id(&session.access_token, "cs1") == Some(device.grant_id.as_str()) => Ok(()),
        None if session.access_token.starts_with("sc_rc_") => Ok(()),
        _ => Err(AuthError::Rejected),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn device_authority_is_bound_and_only_rotating_credential_persists() {
        let auth = ScaffoldAuth::new(ScaffoldConfig::new("https://control.example", "https://edge.example", "project")).unwrap();
        let id = "0123456789abcdef0123456789abcdef";
        let config = DeviceConfig { join_grant: format!("cg1.{id}.enrollment-secret"), device_id: "device".into(), session_id: "session".into(), deployment_id: "deployment".into(), sandbox_id: "sandbox".into(), lifecycle_epoch: 7 };
        let reply = || DeviceReply { access_token: format!("cs1.{id}.rotating-secret"), expires_at: now_ms() + 60_000, user_id: "owner".into(), email: "owner@example.com".into(), project_id: "project".into(), deployment_id: "deployment".into(), sandbox_id: "sandbox".into(), target_device_id: "device".into(), session_id: "session".into(), lifecycle_epoch: 7, capabilities: vec!["session.read".into()] };
        let session = auth.device_session(reply(), &config, id).unwrap();
        for field in ["device", "session", "deployment", "sandbox", "epoch"] {
            let mut changed = config.clone();
            match field {
                "device" => changed.device_id = "other".into(),
                "session" => changed.session_id = "other".into(),
                "deployment" => changed.deployment_id = "other".into(),
                "sandbox" => changed.sandbox_id = "other".into(),
                _ => changed.lifecycle_epoch += 1,
            }
            assert!(matches!(auth.refresh_device(&session, &changed).await, Err(AuthError::Rejected)), "{field}");
        }
        for field in ["sessionId", "deploymentId", "projectId", "sandboxId", "targetDeviceId", "userId"] {
            let mut value = serde_json::json!({"accessToken":format!("cs1.{id}.secret"),"expiresAt":now_ms()+60_000,"userId":"owner","email":"owner@example.com","projectId":"project","deploymentId":"deployment","sandboxId":"sandbox","targetDeviceId":"device","sessionId":"session","lifecycleEpoch":7,"capabilities":["session.read"]});
            value[field] = serde_json::json!(if field == "userId" { "" } else { "other" });
            assert!(auth.device_session(serde_json::from_value(value).unwrap(), &config, id).is_err(), "{field}");
        }
        let mut expired = reply();
        expired.expires_at = now_ms() - 1;
        assert!(auth.device_session(expired, &config, id).is_err());
        let mut stale = reply();
        stale.lifecycle_epoch = 6;
        assert!(auth.device_session(stale, &config, id).is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.json");
        save_session(&path, &session).unwrap();
        let serialized = std::fs::read_to_string(&path).unwrap();
        assert!(!serialized.contains("enrollment-secret"));
        assert_eq!(load_session(&path).unwrap().unwrap().device, session.device);
        let mut enrollment = session;
        enrollment.access_token = config.join_grant.clone();
        assert!(save_session(&path, &enrollment).is_err());
        assert!(!format!("{config:?}").contains("enrollment-secret"));
    }
    #[test]
    fn pkce_and_origin_boundary() {
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        for invalid in ["http://example.com", "https://user@example.com", "https://example.com/path", "https://example.com/?scope=other"] { assert!(normalized_origin(invalid).is_err()); }
        assert!(same_origin("https://example.com", "https://example.com.evil/token").is_err());
        assert!(same_origin("https://example.com", "https://secret@example.com/token").is_err());
    }
    #[test]
    fn callbacks_and_grants_reject_ambiguous_authority() {
        assert!(parse_callback("crew-canary://auth/callback?state=a&state=b&code=c").is_err());
        assert!(parse_callback("http://127.0.0.1:9000/callback?state=a&code=b#fragment").is_err());
        assert_eq!(parse_callback("state.code").unwrap().0, "state");
        assert!(grant_token_id("cs1.0123456789abcdef0123456789abcdef.secret", "cg1").is_none());
    }
    #[tokio::test]
    async fn callback_replay_expiry_and_cancel_are_denied_before_exchange() {
        let auth = ScaffoldAuth::new(ScaffoldConfig::new("https://control.example", "https://edge.example", "project")).unwrap();
        let pending = |created| Pending { created, generation: 0, client_id: "client".into(), redirect_uri: "http://127.0.0.1:1234/callback".into(), verifier: "secret".into(), token_endpoint: "https://control.example/token".into(), resource: "https://control.example".into() };
        lock(&auth.sign_ins).pending.insert("expired".into(), pending(Instant::now() - TTL));
        assert!(matches!(auth.complete_sign_in("expired.code").await, Err(AuthError::Rejected)));
        lock(&auth.sign_ins).pending.insert("wrong-origin".into(), pending(Instant::now()));
        assert!(matches!(auth.complete_sign_in("http://127.0.0.1:9999/callback?state=wrong-origin&code=code").await, Err(AuthError::Rejected)));
        assert!(matches!(auth.complete_sign_in("wrong-origin.code").await, Err(AuthError::Rejected)));
        lock(&auth.sign_ins).pending.insert("cancelled".into(), pending(Instant::now()));
        auth.cancel_sign_in();
        assert!(matches!(auth.complete_sign_in("cancelled.code").await, Err(AuthError::Rejected)));
    }
    #[test]
    fn credential_persistence_is_private_and_profile_is_scope_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let mut session = ScaffoldSession { access_token: "sc_rc_secret".into(), resource: "https://control.example".into(), edge_origin: "https://edge.example".into(), identity: ScaffoldIdentity { user_id: "a/b".into(), email: "user@example.com".into(), name: None, project_scope: "p".into(), capabilities: vec![], deployment_id: None }, device: None };
        save_session(&path, &session).unwrap();
        assert_eq!(load_session(&path).unwrap().unwrap().access_token, session.access_token);
        assert!(!format!("{session:?}").contains("sc_rc_secret"));
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600); }
        let original = profile_key(&session);
        session.identity.user_id = "a_b".into();
        assert_ne!(original, profile_key(&session));
        session.identity.user_id = "a/b".into();
        session.edge_origin = "https://other.example".into();
        assert_ne!(original, profile_key(&session));
    }
}
