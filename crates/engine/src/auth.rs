//! Ashler Comet authentication.
//!
//! Comet is a public OAuth client of Scaffold's IAP-protected control plane. It
//! discovers OAuth metadata, dynamically registers an exact loopback redirect,
//! and uses authorization code + PKCE S256. The resulting permanent, revocable
//! `sc_rc_…` bearer is stored mode 0600 and is never copied into a sandbox.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::EngineError;

const SIGN_IN_TTL: Duration = Duration::from_secs(15 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_OAUTH_SCOPES: &str =
    "remote_code:create remote_code:read remote_code:write remote_code:exec remote_code:lifecycle";
const DEFAULT_INTERNAL_CAPABILITIES: &str = "session.read session.chat session.control session.annotate session.invite session.files session.environment";
const DEVICE_REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const DEVICE_REFRESH_RETRY: Duration = Duration::from_secs(5 * 60);
const STAGING_EDGE_HOST: &str = "comet-staging.internal.ashler.com";
const PRODUCTION_EDGE_HOST: &str = "comet.internal.ashler.com";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthUser {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuthState {
    SignedOut,
    SignedIn {
        user: AuthUser,
        project_scope: String,
    },
}

impl AuthState {
    pub fn is_signed_in(&self) -> bool {
        matches!(self, Self::SignedIn { .. })
    }

    pub fn project_scope(&self) -> Option<&str> {
        match self {
            Self::SignedIn { project_scope, .. } => Some(project_scope),
            Self::SignedOut => None,
        }
    }

    pub fn user(&self) -> Option<&AuthUser> {
        match self {
            Self::SignedIn { user, .. } => Some(user),
            Self::SignedOut => None,
        }
    }

    pub fn to_proto(&self) -> comet_proto::AuthState {
        let profile = |user: &AuthUser| comet_proto::UserProfile {
            id: user.id.clone(),
            email: user.email.clone(),
            name: user.name.clone(),
        };
        match self {
            Self::SignedOut => comet_proto::AuthState::SignedOut,
            Self::SignedIn {
                user,
                project_scope,
            } => comet_proto::AuthState::SignedIn {
                user: profile(user),
                project_scope: project_scope.clone(),
            },
        }
    }
}

impl Serialize for AuthState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_proto().serialize(serializer)
    }
}

#[derive(Clone)]
pub struct AuthConfig {
    pub edge_url: String,
    pub data_dir: PathBuf,
    /// Scaffold control-plane origin. `None` selects explicit local dev mode.
    pub scaffold_url: Option<String>,
    /// Operator-configured deployment/project boundary, never caller input.
    pub project_scope: String,
    /// Platform remote-code scopes requested during Scaffold OAuth.
    pub oauth_scopes: String,
    /// Internal Comet capabilities used by explicit local development auth.
    pub internal_capabilities: String,
    pub dev_user_id: String,
    pub callback_port: Option<u16>,
    /// One-time `cg1` enrollment credential for a sandbox device. It is exchanged
    /// into a renewable, in-memory `cs1` session, never persisted, and never
    /// included in `Debug`.
    pub device_join_grant: Option<String>,
    pub expected_device_id: Option<String>,
    pub expected_session_id: Option<String>,
    pub expected_deployment_id: Option<String>,
    pub expected_sandbox_id: Option<String>,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("edge_url", &self.edge_url)
            .field("data_dir", &self.data_dir)
            .field("scaffold_url", &self.scaffold_url)
            .field("project_scope", &self.project_scope)
            .field("oauth_scopes", &self.oauth_scopes)
            .field("internal_capabilities", &self.internal_capabilities)
            .field("dev_user_id", &"<redacted>")
            .field("callback_port", &self.callback_port)
            .field(
                "device_join_grant",
                &self.device_join_grant.as_ref().map(|_| "<redacted>"),
            )
            .field("expected_device_id", &self.expected_device_id)
            .field("expected_session_id", &self.expected_session_id)
            .field("expected_deployment_id", &self.expected_deployment_id)
            .field("expected_sandbox_id", &self.expected_sandbox_id)
            .finish()
    }
}

impl AuthConfig {
    pub fn new(edge_url: impl Into<String>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            edge_url: edge_url.into(),
            data_dir: data_dir.into(),
            scaffold_url: None,
            project_scope: "ashler-staging".into(),
            oauth_scopes: DEFAULT_OAUTH_SCOPES.into(),
            internal_capabilities: DEFAULT_INTERNAL_CAPABILITIES.into(),
            dev_user_id: "dev-user".into(),
            callback_port: None,
            device_join_grant: None,
            expected_device_id: None,
            expected_session_id: None,
            expected_deployment_id: None,
            expected_sandbox_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSession {
    access_token: String,
    user: AuthUser,
    project_scope: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Debug, Clone)]
struct PendingSignIn {
    created_at: Instant,
    client_id: String,
    redirect_uri: String,
    code_verifier: String,
    token_endpoint: String,
    resource: String,
}

struct AuthInner {
    config: AuthConfig,
    scaffold: Option<String>,
    http: reqwest::Client,
    state_tx: watch::Sender<AuthState>,
    stored: Mutex<Option<StoredSession>>,
    pending: Mutex<HashMap<String, PendingSignIn>>,
    loopback: tokio::sync::Mutex<Option<u16>>,
    device_mode: bool,
}

#[derive(Clone)]
pub struct Auth {
    inner: Arc<AuthInner>,
}

impl Auth {
    pub fn new(config: AuthConfig) -> Self {
        Self::new_with_mode(config, false)
    }

    fn new_with_mode(config: AuthConfig, device_mode: bool) -> Self {
        let scaffold = if device_mode {
            None
        } else {
            config
                .scaffold_url
                .clone()
                .map(|value| value.trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty())
        };
        let stored = if scaffold.is_some() {
            std::fs::read_to_string(config.data_dir.join("session.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<StoredSession>(&raw).ok())
                .filter(|session| {
                    session.project_scope == config.project_scope
                        && session.access_token.starts_with("sc_rc_")
                })
        } else {
            None
        };
        let initial = if device_mode {
            AuthState::SignedOut
        } else {
            match (&scaffold, &stored) {
                (None, _) => AuthState::SignedIn {
                    user: AuthUser {
                        id: config.dev_user_id.clone(),
                        email: format!("{}@dev.local", config.dev_user_id),
                        name: None,
                    },
                    project_scope: config.project_scope.clone(),
                },
                (Some(_), Some(session)) => {
                    signed_in_state(session.user.clone(), &session.project_scope)
                }
                (Some(_), None) => AuthState::SignedOut,
            }
        };
        let (state_tx, _) = watch::channel(initial);
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            inner: Arc::new(AuthInner {
                config,
                scaffold,
                http,
                state_tx,
                stored: Mutex::new(stored),
                pending: Mutex::new(HashMap::new()),
                loopback: tokio::sync::Mutex::new(None),
                device_mode,
            }),
        }
    }

    pub async fn detect(mut config: AuthConfig) -> Self {
        if let Some(grant) = config.device_join_grant.take() {
            let auth = Self::new_with_mode(config, true);
            if let Ok(session) = auth.exchange_device_grant(&grant).await {
                auth.inner.state_tx.send_replace(signed_in_state(
                    session.user.clone(),
                    &session.project_scope,
                ));
                *lock(&auth.inner.stored) = Some(session);
            }
            return auth;
        }
        if config.scaffold_url.is_some() {
            #[derive(Deserialize)]
            struct Health {
                auth: Option<String>,
            }
            let url = format!("{}/health", config.edge_url.trim_end_matches('/'));
            if let Ok(response) = reqwest::Client::new()
                .get(url)
                .timeout(HTTP_TIMEOUT)
                .send()
                .await
                && let Ok(health) = response.json::<Health>().await
                && health.auth.as_deref() == Some("dev")
            {
                config.scaffold_url = None;
            }
        }
        Self::new(config)
    }

    async fn exchange_device_grant(&self, grant: &str) -> Result<StoredSession, EngineError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Exchange {
            access_token: String,
            expires_at: i64,
            user_id: String,
            email: String,
            project_id: String,
            deployment_id: String,
            sandbox_id: String,
            target_device_id: String,
            session_id: String,
            #[serde(default)]
            capabilities: Vec<String>,
        }

        if !grant.starts_with("cg1.") {
            return Err(EngineError::Other("device_join_grant_unavailable".into()));
        }
        let exchange_url = device_grant_exchange_url(&self.inner.config.edge_url)?;
        let response = self
            .inner
            .http
            .post(exchange_url)
            .json(&serde_json::json!({ "grant": grant }))
            .send()
            .await
            .map_err(|_| EngineError::Other("device_join_grant_unavailable".into()))?;
        if !response.status().is_success() {
            return Err(EngineError::Other("device_join_grant_unavailable".into()));
        }
        let exchange: Exchange = response
            .json()
            .await
            .map_err(|_| EngineError::Other("device_join_grant_unavailable".into()))?;
        if !exchange.access_token.starts_with("cs1.")
            || exchange.expires_at <= chrono::Utc::now().timestamp_millis()
            || exchange.project_id != self.inner.config.project_scope
            || self
                .inner
                .config
                .expected_device_id
                .as_deref()
                .is_some_and(|expected| expected != exchange.target_device_id)
            || self
                .inner
                .config
                .expected_session_id
                .as_deref()
                .is_some_and(|expected| expected != exchange.session_id)
            || self
                .inner
                .config
                .expected_deployment_id
                .as_deref()
                .is_some_and(|expected| expected != exchange.deployment_id)
            || self
                .inner
                .config
                .expected_sandbox_id
                .as_deref()
                .is_some_and(|expected| expected != exchange.sandbox_id)
        {
            return Err(EngineError::Other("device_join_grant_unavailable".into()));
        }
        Ok(StoredSession {
            access_token: exchange.access_token,
            user: AuthUser {
                id: exchange.user_id,
                email: exchange.email,
                name: None,
            },
            project_scope: exchange.project_id,
            capabilities: exchange.capabilities,
        })
    }

    pub fn oauth_enabled(&self) -> bool {
        self.inner.scaffold.is_some()
    }

    pub fn device_mode(&self) -> bool {
        self.inner.device_mode
    }

    pub fn watch_state(&self) -> watch::Receiver<AuthState> {
        self.inner.state_tx.subscribe()
    }

    pub fn state(&self) -> AuthState {
        self.inner.state_tx.borrow().clone()
    }

    /// Internal Comet capabilities available to the current principal. Local
    /// development uses the configured internal capability list; authenticated
    /// Scaffold scopes are translated into the corresponding internal vocabulary.
    pub fn capabilities(&self) -> Vec<String> {
        if self.inner.scaffold.is_none() && !self.inner.device_mode {
            return self
                .inner
                .config
                .internal_capabilities
                .split_whitespace()
                .map(str::to_string)
                .collect();
        }
        lock(&self.inner.stored)
            .as_ref()
            .map(|session| session.capabilities.clone())
            .unwrap_or_default()
    }

    pub fn user_id(&self) -> Option<String> {
        if self.inner.device_mode {
            return lock(&self.inner.stored)
                .as_ref()
                .map(|session| session.user.id.clone());
        }
        if self.inner.scaffold.is_none() {
            let dev = &self.inner.config.dev_user_id;
            return Some(dev.split('@').next().unwrap_or(dev).to_string());
        }
        self.state().user().map(|user| user.id.clone())
    }

    pub async fn access_token(&self) -> Option<String> {
        if self.inner.device_mode {
            return lock(&self.inner.stored)
                .as_ref()
                .map(|session| session.access_token.clone());
        }
        if self.inner.scaffold.is_none() {
            return Some(self.inner.config.dev_user_id.clone());
        }
        lock(&self.inner.stored)
            .as_ref()
            .map(|session| session.access_token.clone())
    }

    /// Device access tokens rotate before their twelve-hour expiry. Scaffold
    /// user bearers remain permanent and do not need a refresh task.
    pub fn spawn_refresh_loop(&self) -> tokio::task::JoinHandle<()> {
        let auth = self.clone();
        tokio::spawn(async move {
            if !auth.inner.device_mode {
                return;
            }
            let mut delay = DEVICE_REFRESH_INTERVAL;
            loop {
                tokio::time::sleep(delay).await;
                delay = match auth.refresh_device_token().await {
                    Ok(()) => DEVICE_REFRESH_INTERVAL,
                    Err(error) => {
                        tracing::warn!(%error, "auth: device token refresh failed");
                        DEVICE_REFRESH_RETRY
                    }
                };
            }
        })
    }

    async fn refresh_device_token(&self) -> Result<(), EngineError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Refresh {
            access_token: String,
            expires_at: i64,
            grant_id: String,
            user_id: String,
            email: String,
            project_id: String,
            deployment_id: String,
            sandbox_id: String,
            target_device_id: String,
            session_id: String,
            #[serde(default)]
            capabilities: Vec<String>,
        }

        let current_session = lock(&self.inner.stored)
            .clone()
            .ok_or_else(|| EngineError::Other("device_token_unavailable".into()))?;
        let current = current_session.access_token.clone();
        let response = self
            .inner
            .http
            .post(format!(
                "{}/auth/device-token/refresh",
                self.inner.config.edge_url.trim_end_matches('/')
            ))
            .bearer_auth(&current)
            .send()
            .await
            .map_err(|error| EngineError::Other(format!("device token refresh failed: {error}")))?;
        if !response.status().is_success() {
            return Err(EngineError::Other(format!(
                "device token refresh rejected ({})",
                response.status().as_u16()
            )));
        }
        let refreshed: Refresh = response.json().await.map_err(|error| {
            EngineError::Other(format!("malformed device token refresh: {error}"))
        })?;
        if !refreshed.access_token.starts_with("cs1.")
            || refreshed.expires_at <= chrono::Utc::now().timestamp_millis()
            || refreshed.grant_id.trim().is_empty()
            || refreshed.user_id != current_session.user.id
            || refreshed.email != current_session.user.email
            || refreshed.project_id != current_session.project_scope
            || refreshed.capabilities != current_session.capabilities
            || self
                .inner
                .config
                .expected_device_id
                .as_deref()
                .is_none_or(|expected| expected != refreshed.target_device_id)
            || self
                .inner
                .config
                .expected_session_id
                .as_deref()
                .is_none_or(|expected| expected != refreshed.session_id)
            || self
                .inner
                .config
                .expected_deployment_id
                .as_deref()
                .is_none_or(|expected| expected != refreshed.deployment_id)
            || self
                .inner
                .config
                .expected_sandbox_id
                .as_deref()
                .is_none_or(|expected| expected != refreshed.sandbox_id)
        {
            return Err(EngineError::Other(
                "device token refresh violated the edge contract".into(),
            ));
        }
        let mut stored = lock(&self.inner.stored);
        let session = stored
            .as_mut()
            .filter(|session| session.access_token == current)
            .ok_or_else(|| EngineError::Other("device token changed during refresh".into()))?;
        session.access_token = refreshed.access_token;
        Ok(())
    }

    pub async fn start_sign_in(&self) -> Result<String, EngineError> {
        if self.inner.scaffold.is_none() {
            return Ok(String::new());
        }
        let port = self.ensure_loopback().await?;
        self.begin_sign_in(&format!("http://127.0.0.1:{port}/callback"))
            .await
    }

    /// A remote browser may fail to reach this machine's loopback callback. In
    /// that case the user pastes its final localhost URL into the terminal.
    pub async fn start_headless_sign_in(&self) -> Result<String, EngineError> {
        self.start_sign_in().await
    }

    pub async fn complete_sign_in(&self, pasted: &str) -> Result<(), EngineError> {
        if self.inner.scaffold.is_none() {
            return Ok(());
        }
        let trimmed = pasted.trim();
        let (state, code) = if trimmed.starts_with("http://127.0.0.1:")
            || trimmed.starts_with("http://localhost:")
        {
            let url = reqwest::Url::parse(trimmed)
                .map_err(|_| EngineError::Other("invalid sign-in callback URL".into()))?;
            let value = |key: &str| {
                url.query_pairs()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.into_owned())
            };
            (
                value("state")
                    .ok_or_else(|| EngineError::Other("callback is missing state".into()))?,
                value("code")
                    .ok_or_else(|| EngineError::Other("callback is missing code".into()))?,
            )
        } else {
            let (state, code) = trimmed.split_once('.').ok_or_else(|| {
                EngineError::Other("paste the full callback URL or state.code".into())
            })?;
            (state.to_string(), code.to_string())
        };
        let pending = self.take_pending(&state).ok_or_else(|| {
            EngineError::Other("sign-in code does not match this device or has expired".into())
        })?;
        let result = self.exchange_code(&pending, &code).await?;
        self.finish_sign_in(result);
        Ok(())
    }

    pub fn sign_out(&self) {
        *lock(&self.inner.stored) = None;
        if !self.inner.device_mode {
            self.persist::<&StoredSession>(None);
        }
        self.inner.state_tx.send_replace(AuthState::SignedOut);
    }

    async fn begin_sign_in(&self, redirect_uri: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct ProtectedResource {
            resource: String,
            authorization_servers: Vec<String>,
            #[serde(default)]
            scopes_supported: Vec<String>,
        }
        #[derive(Deserialize)]
        struct AuthorizationServer {
            issuer: String,
            authorization_endpoint: String,
            token_endpoint: String,
            registration_endpoint: String,
            code_challenge_methods_supported: Vec<String>,
        }
        #[derive(Deserialize)]
        struct Registration {
            client_id: String,
        }

        let scaffold = self.inner.scaffold.as_deref().unwrap_or_default();
        let protected: ProtectedResource = self
            .get_json(
                &format!("{scaffold}/.well-known/oauth-protected-resource"),
                "OAuth resource metadata",
            )
            .await?;
        require_same_origin(scaffold, &protected.resource, "OAuth resource")?;
        let issuer = protected.authorization_servers.first().ok_or_else(|| {
            EngineError::Other("OAuth metadata has no authorization server".into())
        })?;
        require_same_origin(scaffold, issuer, "OAuth issuer")?;
        let server: AuthorizationServer = self
            .get_json(
                &format!(
                    "{}/.well-known/oauth-authorization-server",
                    issuer.trim_end_matches('/')
                ),
                "OAuth authorization metadata",
            )
            .await?;
        require_same_origin(scaffold, &server.issuer, "OAuth issuer")?;
        for (label, endpoint) in [
            (
                "authorization endpoint",
                server.authorization_endpoint.as_str(),
            ),
            ("token endpoint", server.token_endpoint.as_str()),
            (
                "registration endpoint",
                server.registration_endpoint.as_str(),
            ),
        ] {
            require_same_origin(scaffold, endpoint, label)?;
        }
        if !server
            .code_challenge_methods_supported
            .iter()
            .any(|method| method == "S256")
        {
            return Err(EngineError::Other(
                "OAuth server does not support PKCE S256".into(),
            ));
        }
        if !required_scopes_present(&self.inner.config.oauth_scopes, &protected.scopes_supported) {
            return Err(EngineError::Other(
                "Scaffold does not advertise the required remote-code OAuth scopes".into(),
            ));
        }

        let response = self
            .inner
            .http
            .post(&server.registration_endpoint)
            .json(&serde_json::json!({
                "redirect_uris": [redirect_uri],
                "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code"],
                "response_types": ["code"]
            }))
            .send()
            .await
            .map_err(|error| EngineError::Other(format!("OAuth registration failed: {error}")))?;
        if !response.status().is_success() {
            return Err(EngineError::Other(format!(
                "OAuth registration failed ({})",
                response.status().as_u16()
            )));
        }
        let registration: Registration = response.json().await.map_err(|error| {
            EngineError::Other(format!("malformed OAuth registration: {error}"))
        })?;
        if registration.client_id.trim().is_empty() {
            return Err(EngineError::Other(
                "OAuth registration returned no client id".into(),
            ));
        }

        let verifier = pkce_verifier();
        let state = uuid::Uuid::new_v4().to_string();
        lock(&self.inner.pending).insert(
            state.clone(),
            PendingSignIn {
                created_at: Instant::now(),
                client_id: registration.client_id.clone(),
                redirect_uri: redirect_uri.to_string(),
                code_verifier: verifier.clone(),
                token_endpoint: server.token_endpoint,
                resource: protected.resource.clone(),
            },
        );
        let mut authorize = reqwest::Url::parse(&server.authorization_endpoint)
            .map_err(|_| EngineError::Other("OAuth authorization endpoint is invalid".into()))?;
        authorize
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &registration.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", &state)
            .append_pair("code_challenge", &pkce_challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", &protected.resource)
            .append_pair("scope", &self.inner.config.oauth_scopes);
        Ok(authorize.to_string())
    }

    fn take_pending(&self, state: &str) -> Option<PendingSignIn> {
        let mut pending = lock(&self.inner.pending);
        let now = Instant::now();
        pending.retain(|_, value| now.duration_since(value.created_at) < SIGN_IN_TTL);
        pending.remove(state)
    }

    async fn exchange_code(
        &self,
        pending: &PendingSignIn,
        code: &str,
    ) -> Result<SignInResult, EngineError> {
        #[derive(Deserialize)]
        struct Tokens {
            access_token: String,
            token_type: String,
            scope: String,
            resource: String,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct SessionActor {
            sub: String,
            auth: String,
            #[serde(default)]
            display_name: Option<String>,
        }
        #[derive(Deserialize)]
        struct Session {
            ok: bool,
            resource: String,
            actor: SessionActor,
            scopes: Vec<String>,
        }

        let response = self
            .inner
            .http
            .post(&pending.token_endpoint)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", pending.client_id.as_str()),
                ("redirect_uri", pending.redirect_uri.as_str()),
                ("code_verifier", pending.code_verifier.as_str()),
                ("resource", pending.resource.as_str()),
            ])
            .send()
            .await
            .map_err(|error| EngineError::Other(format!("token exchange failed: {error}")))?;
        if !response.status().is_success() {
            return Err(EngineError::Other(format!(
                "token exchange failed ({})",
                response.status().as_u16()
            )));
        }
        let tokens: Tokens = response
            .json()
            .await
            .map_err(|error| EngineError::Other(format!("malformed token response: {error}")))?;
        let granted: Vec<String> = tokens
            .scope
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if tokens.token_type != "Bearer"
            || !tokens.access_token.starts_with("sc_rc_")
            || tokens.resource != pending.resource
            || !required_scopes_present(&self.inner.config.oauth_scopes, &granted)
        {
            return Err(EngineError::Other(
                "OAuth token response violated the Scaffold contract".into(),
            ));
        }

        let response = self
            .inner
            .http
            .get(format!(
                "{}/api/code-sandboxes/auth/session",
                pending.resource.trim_end_matches('/')
            ))
            .bearer_auth(&tokens.access_token)
            .send()
            .await
            .map_err(|error| EngineError::Other(format!("identity validation failed: {error}")))?;
        if !response.status().is_success() {
            return Err(EngineError::Other(
                "Scaffold rejected the issued bearer".into(),
            ));
        }
        let session: Session = response
            .json()
            .await
            .map_err(|error| EngineError::Other(format!("malformed identity response: {error}")))?;
        if !session.ok
            || session.resource != pending.resource
            || session.actor.auth != "iap"
            || session.actor.sub.trim().is_empty()
            || !required_scopes_present(&self.inner.config.oauth_scopes, &session.scopes)
        {
            return Err(EngineError::Other(
                "Scaffold identity response was not authorized".into(),
            ));
        }
        Ok(SignInResult {
            access_token: tokens.access_token,
            user: AuthUser {
                id: session.actor.sub.clone(),
                email: session.actor.sub,
                name: session.actor.display_name,
            },
            capabilities: capabilities_from_remote_code_scopes(&session.scopes),
        })
    }

    fn finish_sign_in(&self, result: SignInResult) {
        let session = StoredSession {
            access_token: result.access_token,
            user: result.user.clone(),
            project_scope: self.inner.config.project_scope.clone(),
            capabilities: result.capabilities,
        };
        self.persist(Some(&session));
        *lock(&self.inner.stored) = Some(session);
        self.inner.state_tx.send_replace(signed_in_state(
            result.user,
            &self.inner.config.project_scope,
        ));
    }

    fn persist<S: std::borrow::Borrow<StoredSession>>(&self, session: Option<S>) {
        let path = self.inner.config.data_dir.join("session.json");
        let outcome = match session {
            Some(session) => std::fs::create_dir_all(&self.inner.config.data_dir).and_then(|()| {
                serde_json::to_vec(session.borrow())
                    .map_err(std::io::Error::other)
                    .and_then(|bytes| write_private(&path, &bytes))
            }),
            None => match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        };
        if let Err(error) = outcome {
            tracing::warn!(%error, path = %path.display(), "auth: could not persist session");
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        label: &str,
    ) -> Result<T, EngineError> {
        let response = self
            .inner
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| EngineError::Other(format!("{label} failed: {error}")))?;
        if !response.status().is_success() {
            return Err(EngineError::Other(format!(
                "{label} failed ({})",
                response.status().as_u16()
            )));
        }
        response
            .json()
            .await
            .map_err(|error| EngineError::Other(format!("malformed {label}: {error}")))
    }

    async fn ensure_loopback(&self) -> Result<u16, EngineError> {
        let mut slot = self.inner.loopback.lock().await;
        if let Some(port) = *slot {
            return Ok(port);
        }
        let listener = tokio::net::TcpListener::bind(format!(
            "127.0.0.1:{}",
            self.inner.config.callback_port.unwrap_or(0)
        ))
        .await
        .map_err(|error| EngineError::Other(format!("could not bind sign-in callback: {error}")))?;
        let port = listener
            .local_addr()
            .map_err(|error| {
                EngineError::Other(format!("could not inspect sign-in callback: {error}"))
            })?
            .port();
        *slot = Some(port);
        tokio::spawn(loopback_loop(listener, Arc::downgrade(&self.inner)));
        Ok(port)
    }
}

struct SignInResult {
    user: AuthUser,
    access_token: String,
    capabilities: Vec<String>,
}

fn signed_in_state(user: AuthUser, project_scope: &str) -> AuthState {
    AuthState::SignedIn {
        user,
        project_scope: project_scope.to_string(),
    }
}

fn required_scopes_present(required: &str, granted: &[String]) -> bool {
    required
        .split_whitespace()
        .all(|scope| granted.iter().any(|candidate| candidate == scope))
}

fn capabilities_from_remote_code_scopes(scopes: &[String]) -> Vec<String> {
    let has_scope = |required: &str| scopes.iter().any(|scope| scope == required);
    [
        ("session.read", has_scope("remote_code:read")),
        ("session.chat", has_scope("remote_code:write")),
        (
            "session.control",
            has_scope("remote_code:exec") || has_scope("remote_code:lifecycle"),
        ),
        ("session.annotate", has_scope("remote_code:write")),
        ("session.invite", has_scope("remote_code:create")),
        ("session.files", has_scope("remote_code:write")),
        (
            "session.environment",
            has_scope("remote_code:exec") || has_scope("remote_code:lifecycle"),
        ),
    ]
    .into_iter()
    .filter(|&(_, granted)| granted)
    .map(|(capability, _)| capability.to_string())
    .collect()
}

fn device_grant_exchange_url(edge_url: &str) -> Result<reqwest::Url, EngineError> {
    let mut url = reqwest::Url::parse(edge_url).map_err(|_| {
        EngineError::Other(
            "Comet edge URL must be an approved exact origin (HTTP is loopback-only)".into(),
        )
    })?;
    let approved_remote = url.scheme() == "https"
        && url.port().is_none()
        && url
            .host_str()
            .is_some_and(|host| host == STAGING_EDGE_HOST || host == PRODUCTION_EDGE_HOST);
    let approved_local = url.scheme() == "http"
        && matches!(
            url.host_str(),
            Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
        );
    let exact_origin = matches!(url.path(), "" | "/")
        && (edge_url == url.as_str()
            || url
                .as_str()
                .strip_suffix('/')
                .is_some_and(|without_slash| edge_url == without_slash));
    if (!approved_remote && !approved_local)
        || !exact_origin
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(EngineError::Other(
            "Comet edge URL must be an approved exact origin (HTTP is loopback-only)".into(),
        ));
    }
    url.set_path("/auth/device-grants/exchange");
    Ok(url)
}

fn require_same_origin(base: &str, candidate: &str, label: &str) -> Result<(), EngineError> {
    let base = reqwest::Url::parse(base)
        .map_err(|_| EngineError::Other("Scaffold control-plane URL is invalid".into()))?;
    let candidate = reqwest::Url::parse(candidate)
        .map_err(|_| EngineError::Other(format!("{label} is invalid")))?;
    let local = matches!(base.host_str(), Some("127.0.0.1" | "localhost"));
    if (!local && candidate.scheme() != "https")
        || base.scheme() != candidate.scheme()
        || base.host_str() != candidate.host_str()
        || base.port_or_known_default() != candidate.port_or_known_default()
    {
        return Err(EngineError::Other(format!(
            "{label} crossed the configured origin"
        )));
    }
    Ok(())
}

fn pkce_verifier() -> String {
    let random = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random.as_bytes())
}

fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[async_trait::async_trait]
impl comet_rpc::TokenSource for Auth {
    async fn token(&self) -> Option<String> {
        if self.inner.scaffold.is_some() && !self.state().is_signed_in() {
            return None;
        }
        self.access_token().await
    }
}

async fn loopback_loop(listener: tokio::net::TcpListener, inner: Weak<AuthInner>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let Some(inner) = inner.upgrade() else {
            break;
        };
        tokio::spawn(async move {
            let _ = handle_loopback_conn(stream, Auth { inner }).await;
        });
    }
}

async fn handle_loopback_conn(
    mut stream: tokio::net::TcpStream,
    auth: Auth,
) -> Result<(), std::io::Error> {
    let mut buffer = [0u8; 8192];
    let read = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..read]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).ok();
    let query = |key: &str| {
        url.as_ref().and_then(|url| {
            url.query_pairs()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.into_owned())
        })
    };
    let (status, body) = match (query("code"), query("state")) {
        (Some(code), Some(state)) => match auth.take_pending(&state) {
            Some(pending) => match auth.exchange_code(&pending, &code).await {
                Ok(result) => {
                    auth.finish_sign_in(result);
                    (
                        "200 OK",
                        page("Signed in to Ashler Comet. You can close this window."),
                    )
                }
                Err(error) => {
                    tracing::warn!(%error, "auth: loopback token exchange failed");
                    (
                        "502 Bad Gateway",
                        page("Sign-in failed. Return to Ashler Comet and try again."),
                    )
                }
            },
            None => (
                "400 Bad Request",
                page("This sign-in link is invalid or expired."),
            ),
        },
        _ => (
            "400 Bad Request",
            page("This callback is missing its sign-in code."),
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn page(message: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset='utf-8'><title>Ashler Comet sign in</title></head><body style='font-family:sans-serif;padding:2rem'>{message}</body></html>"
    )
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_uses_url_safe_s256() {
        let verifier = pkce_verifier();
        assert!(verifier.len() >= 43);
        assert!(!verifier.contains('='));
        assert_eq!(pkce_challenge(&verifier).len(), 43);
    }

    #[test]
    fn capabilities_fail_closed() {
        let granted = vec!["session.read".to_string(), "session.chat".to_string()];
        assert!(required_scopes_present("session.read", &granted));
        assert!(!required_scopes_present(
            "session.read session.control",
            &granted
        ));
    }

    #[test]
    fn oauth_defaults_use_platform_scopes_while_dev_keeps_internal_capabilities() {
        let mut config = AuthConfig::new("http://localhost:27640", PathBuf::new());
        assert_eq!(
            config.oauth_scopes,
            "remote_code:create remote_code:read remote_code:write remote_code:exec remote_code:lifecycle"
        );
        config.internal_capabilities = "session.read session.files".into();

        let auth = Auth::new(config);
        assert_eq!(auth.capabilities().join(" "), "session.read session.files");
    }

    #[test]
    fn platform_scopes_map_to_least_privilege_internal_capabilities() {
        let all_scopes = DEFAULT_OAUTH_SCOPES
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            capabilities_from_remote_code_scopes(&all_scopes).join(" "),
            "session.read session.chat session.control session.annotate session.invite session.files session.environment"
        );
        assert_eq!(
            capabilities_from_remote_code_scopes(&["remote_code:read".into()]).join(" "),
            "session.read"
        );
    }

    #[test]
    fn device_grant_exchange_uses_only_approved_edge_origins() {
        assert_eq!(
            device_grant_exchange_url("https://comet.internal.ashler.com")
                .unwrap()
                .as_str(),
            "https://comet.internal.ashler.com/auth/device-grants/exchange"
        );
        assert_eq!(
            device_grant_exchange_url("https://comet-staging.internal.ashler.com")
                .unwrap()
                .as_str(),
            "https://comet-staging.internal.ashler.com/auth/device-grants/exchange"
        );
        assert_eq!(
            device_grant_exchange_url("http://127.0.0.1:8787")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:8787/auth/device-grants/exchange"
        );
        assert_eq!(
            device_grant_exchange_url("http://localhost:8787/")
                .unwrap()
                .as_str(),
            "http://localhost:8787/auth/device-grants/exchange"
        );
        assert_eq!(
            device_grant_exchange_url("http://[::1]:8787")
                .unwrap()
                .as_str(),
            "http://[::1]:8787/auth/device-grants/exchange"
        );
    }

    #[test]
    fn device_grant_exchange_rejects_unapproved_edge_origins() {
        for edge_url in [
            "https://attacker.example",
            "https://comet.internal.ashler.com.attacker.example",
            "http://comet.internal.ashler.com",
            "http://192.0.2.1:8787",
            "https://user@comet.internal.ashler.com",
            "https://comet.internal.ashler.com/unexpected",
            "https://comet.internal.ashler.com/unexpected/../",
            "https://comet.internal.ashler.com:444/",
            "https://comet.internal.ashler.com?redirect=https://attacker.example",
            "https://comet.internal.ashler.com#unexpected",
            "ftp://comet.internal.ashler.com",
            "https://localhost:8787",
        ] {
            assert!(
                device_grant_exchange_url(edge_url).is_err(),
                "{edge_url} must not receive a device grant"
            );
        }
    }

    #[tokio::test]
    async fn rejected_edge_origin_error_does_not_expose_device_grant() {
        let auth = Auth::new_with_mode(AuthConfig::new("not a URL", PathBuf::new()), true);
        let grant = "cg1.must-not-appear";

        let error = auth.exchange_device_grant(grant).await.unwrap_err();

        assert!(!error.to_string().contains(grant));
        assert!(!auth.watch_state().borrow().is_signed_in());
    }

    #[test]
    fn discovery_cannot_cross_origin() {
        assert!(
            require_same_origin(
                "https://scaffold-staging.internal.ashler.com",
                "https://scaffold-staging.internal.ashler.com/api/oauth/token",
                "token endpoint"
            )
            .is_ok()
        );
        assert!(
            require_same_origin(
                "https://scaffold-staging.internal.ashler.com",
                "https://evil.example/api/oauth/token",
                "token endpoint"
            )
            .is_err()
        );
    }
}
