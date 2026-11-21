//! Crew: Scaffold's code sandboxes, as a desktop creates and attaches them
//! (docs/crew/zgx-thin-port.md, "Scaffold sandboxes", piece 3), trimmed from
//! Crew's `crates/engine/src/scaffold.rs` (Ashler-AI/comet):
//!
//! - [`ScaffoldClient`]: Platform's `/api/code-sandboxes` (list, inspect,
//!   create, exec, files) and the checks every answer passes before anything
//!   trusts it ([`Sandbox::into_environment`]);
//! - [`EdgeDeviceJoinGrantClient`]: this engine's Edge's device grants
//!   (`POST /auth/device-grants`), Crew's seven capabilities in Crew's order;
//! - [`SandboxHosts::attach`]: probe the sandbox's `scaffold-authority` and
//!   reuse an exact match, else mint a grant, hand it over in a private file
//!   and start the host with the native runner's exact argv, then wait for
//!   the probe to confirm it.
//!
//! Left out for now: handoff, recovery, pause, resume, stop, start (a new
//! epoch), Boat and the crew directory.
//!
//! The `sc_rc_` bearer comes from the engine's verified profile
//! ([`crate::auth::ProfileTokens`]) on every request, and is only ever an
//! `Authorization` header to the authority's origin or this engine's Edge:
//! never across a redirect, never in a sandbox's exec input, an error or a
//! log. A sandbox gets only a short-lived device-join grant, in a file only
//! its owner can read.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeron_harness::CancellationToken;
use zeron_proto::HarnessId;
use zeron_rpc::{TokenError, TokenSource};

use crate::auth::{DeviceGrantAuthority, ScaffoldScope};
use crate::now_ms;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RUNTIME_START_POLL: Duration = Duration::from_millis(500);
/// At 30 s a request, two retries cap a persistent timeout at about 90 s.
const ATTACH_PROBE_TRANSPORT_RETRIES: usize = 2;
const JOIN_GRANT_TTL_SECONDS: u32 = 15 * 60;
const DEVICE_ACCESS_TTL_MS: i64 = 12 * 60 * 60 * 1000;
/// Platform's answers are small; anything bigger isn't one.
const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
/// The authority profile's wire (`cometRuntimeProfile.version`), not the
/// runtime archive's (`scaffold.comet-runtime.v2`).
pub(crate) const PROFILE_VERSION: &str = "scaffold.comet-runtime.v1";
const JOIN_GRANT_PATH: [&str; 2] = ["auth", "device-grants"];

/// Where a sandbox's checkout is, and its chat's runs go.
pub(crate) const HOST_CWD: &str = "/workspace/ashler-platform";
/// The repository every sandbox clones.
pub(crate) const HOST_REPO: &str = "Ashler-AI/ashler-platform";
/// The host as the native runner installs it (`native-crew-runner.mjs`).
const HOST_EXECUTABLE: &str = ".scaffold/crew/bin/comet";
const HOST_DATA_DIR: &str = ".scaffold/crew-data";
const HOST_IPC_PORT: &str = "39400";

/// A sandbox host's device: `comet-scaffold-{sandbox}-e{epoch}`.
pub(crate) fn device_id_for(sandbox_id: &str, lifecycle_epoch: u64) -> String {
    format!("comet-scaffold-{sandbox_id}-e{lifecycle_epoch}")
}

/// The sandbox and epoch a host's device id names.
pub(crate) fn parse_device_id(device_id: &str) -> Option<(&str, u64)> {
    let target = device_id.strip_prefix("comet-scaffold-")?;
    let (sandbox_id, epoch) = target.rsplit_once("-e")?;
    let epoch = epoch.parse::<u64>().ok().filter(|epoch| *epoch >= 1)?;
    (scaffold_id(sandbox_id) && device_id == device_id_for(sandbox_id, epoch)).then_some((sandbox_id, epoch))
}

/// Scaffold's id pattern (`^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$`).
pub(crate) fn scaffold_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .enumerate()
            .all(|(index, byte)| byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'.' | b'_' | b':' | b'-')))
}

/// What a host's grant carries, in Crew's order (the host echoes the Edge's).
pub(crate) fn host_capabilities() -> Vec<String> {
    [
        "session.read",
        "session.sync.write",
        "session.chat",
        "session.control",
        "session.annotate",
        "session.files",
        "session.environment",
    ]
    .map(String::from)
    .to_vec()
}

// -- wire types (Crew's `zeron_proto` collaboration types, which upstream's
// proto doesn't have) ---------------------------------------------------------

/// A sandbox's lifecycle, as Platform reports it. No catch-all: a status this
/// build doesn't know fails decoding rather than reading as a safe one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Lifecycle {
    Creating,
    RestoringSnapshot,
    Starting,
    Ready,
    AgentRunning,
    Paused,
    Resuming,
    Stopped,
    Failed,
}

impl Lifecycle {
    /// A host can be started, or found, in it.
    fn attachable(self) -> bool {
        matches!(
            self,
            Self::Creating | Self::RestoringSnapshot | Self::Starting | Self::Resuming | Self::Ready | Self::AgentRunning
        )
    }

    pub(crate) fn terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Failed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DatabaseEnvironment {
    #[default]
    Local,
    StagingSnapshot,
    ProductionSnapshot,
}

/// Where a session runs (Crew's `SessionEnvironmentSource`). Its fields keep
/// Crew's snake_case inside the camelCase tag: the Edge reads them so
/// (`edge/src/crew/registry-scope.ts`, `bindingMatches`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum EnvironmentSource {
    Local,
    Scaffold {
        sandbox_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        lifecycle: Lifecycle,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lifecycle_epoch: Option<u64>,
        #[serde(default)]
        links: serde_json::Map<String, Value>,
    },
}

/// A session's sandbox, verified (Crew's `SessionEnvironment`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionEnvironment {
    pub source: EnvironmentSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Platform's verified owner (its email); a published row says the
    /// verified subject instead.
    pub owner_principal: String,
    pub scope: ScaffoldScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_environment: Option<DatabaseEnvironment>,
    /// Crew keeps it among the environment's unknown fields, on the same key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_route: Option<AgentRoute>,
}

impl SessionEnvironment {
    /// Its sandbox, lifecycle and epoch.
    pub(crate) fn sandbox(&self) -> Result<(&str, Lifecycle, Option<u64>), ScaffoldError> {
        match &self.source {
            EnvironmentSource::Scaffold {
                sandbox_id,
                lifecycle,
                lifecycle_epoch,
                ..
            } => Ok((sandbox_id, *lifecycle, *lifecycle_epoch)),
            EnvironmentSource::Local => Err(ScaffoldError::InvalidResponse("a local environment has no sandbox".into())),
        }
    }

    pub(crate) fn region(&self) -> Option<&str> {
        match &self.source {
            EnvironmentSource::Scaffold { region, .. } => region.as_deref(),
            EnvironmentSource::Local => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AgentProvider {
    OpenAi,
    Anthropic,
    Antigravity,
}

impl AgentProvider {
    /// The provider prefix of this port's OMP model ids.
    fn omp_prefix(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Antigravity => "google-antigravity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AgentRoutingMode {
    Automatic,
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentRouteFallback {
    Disabled,
}

/// Credential-free inference intent (Crew's `AgentRoute`). `account_id` is an
/// opaque Agent Auth account, never a credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentRoute {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessId>,
    pub provider: AgentProvider,
    pub model: String,
    pub fallback: AgentRouteFallback,
    pub routing_mode: AgentRoutingMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl AgentRoute {
    pub(crate) fn automatic(provider: AgentProvider, model: impl Into<String>) -> Self {
        Self {
            harness: None,
            provider,
            model: model.into(),
            fallback: AgentRouteFallback::Disabled,
            routing_mode: AgentRoutingMode::Automatic,
            account_id: None,
        }
    }

    /// The route for a chat's agent and model, hosted on OMP: OMP's own
    /// provider-qualified ids (`anthropic/…`, `openai/…`, `openai-codex/…`),
    /// or a Claude Code or Codex model on its provider. `account` pins one
    /// shared account (the chat's `crewAccountId`).
    pub(crate) fn for_chat(harness: &HarnessId, model: &str, account: Option<&str>) -> Option<Self> {
        let model = model.trim();
        let lower = model.to_ascii_lowercase();
        let (provider, model) = match harness {
            HarnessId::Omp => {
                let (prefix, model) = model.split_once('/')?;
                let provider = match prefix.to_ascii_lowercase().as_str() {
                    "anthropic" => AgentProvider::Anthropic,
                    "openai" | "openai-codex" => AgentProvider::OpenAi,
                    _ => return None,
                };
                (provider, model)
            }
            HarnessId::ClaudeCode if lower.starts_with("claude-") => (AgentProvider::Anthropic, model),
            HarnessId::Codex if lower.starts_with("gpt-") => (AgentProvider::OpenAi, model),
            _ => return None,
        };
        if model.is_empty() || model.contains('/') {
            return None;
        }
        let mut route = Self::automatic(provider, model);
        if let Some(account) = account.map(str::trim).filter(|account| !account.is_empty()) {
            route.routing_mode = AgentRoutingMode::Pinned;
            route.account_id = Some(account.to_owned());
        }
        route.validate().ok()?;
        Some(route)
    }

    /// The harness a sandbox runs it on: OMP, unless the route names Claude
    /// Code or Codex.
    pub(crate) fn harness(&self) -> HarnessId {
        self.harness.clone().unwrap_or(HarnessId::Omp)
    }

    /// The model as that harness names it: OMP's provider-qualified id.
    pub(crate) fn execution_model(&self) -> String {
        match self.harness() {
            HarnessId::Omp => format!("{}/{}", self.provider.omp_prefix(), self.model),
            _ => self.model.clone(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        let valid_harness = matches!(
            (&self.harness, self.provider),
            (None, _)
                | (Some(HarnessId::Omp), AgentProvider::Antigravity)
                | (Some(HarnessId::ClaudeCode), AgentProvider::Anthropic)
                | (Some(HarnessId::Codex), AgentProvider::OpenAi)
        );
        if !valid_harness {
            return Err("agent route harness does not match provider");
        }
        if self.model.trim().is_empty() {
            return Err("agent route model is required");
        }
        if self.harness() != HarnessId::Omp && self.model.contains('/') {
            return Err("native agent route model must not be provider-qualified");
        }
        match (self.routing_mode, self.account_id.as_deref()) {
            (AgentRoutingMode::Automatic, None) => Ok(()),
            (AgentRoutingMode::Automatic, Some(_)) => Err("automatic agent route cannot select an account"),
            (AgentRoutingMode::Pinned, Some(account)) if !account.trim().is_empty() => Ok(()),
            (AgentRoutingMode::Pinned, _) => Err("pinned agent route requires an account id"),
        }
    }
}

/// A host's confirmed grant (Crew's `ScaffoldControlGrant`): never its secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlGrant {
    pub id: String,
    pub expires_at: i64,
    pub capabilities: Vec<String>,
}

/// An attached host: its device, the supervised run that started it (none
/// when an exact host was already serving) and its grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Attachment {
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub grant: ControlGrant,
}

// -- errors --------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: Option<String>,
    pub retry_after: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportCause {
    RequestTimeout,
    ConnectFailure,
    ConnectionResetOrClosed,
    SendFailure,
}

impl fmt::Display for TransportCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::RequestTimeout => "request_timeout",
            Self::ConnectFailure => "connect_failure",
            Self::ConnectionResetOrClosed => "connection_reset_or_closed",
            Self::SendFailure => "send_failure",
        })
    }
}

/// Only the status, code and Platform's own message: never a body, a request
/// or a credential.
pub(crate) struct ApiErrorDisplay(pub ApiError);

impl fmt::Debug for ApiErrorDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for ApiErrorDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "scaffold_api_error:{}:{}", self.0.status, self.0.code)?;
        if let Some(message) = &self.0.message {
            write!(f, ":{message}")?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ScaffoldError {
    #[error("scaffold_auth_unavailable")]
    AuthUnavailable,
    #[error("scaffold_auth_temporarily_unavailable")]
    AuthTemporarilyUnavailable,
    #[error("scaffold_request_cancelled")]
    Cancelled,
    #[error("scaffold_request_failed: {cause}")]
    Transport {
        cause: TransportCause,
        /// Nothing came back: the request may or may not have been seen.
        before_response: bool,
        #[source]
        source: reqwest::Error,
    },
    #[error("scaffold_response_invalid: {0}")]
    InvalidResponse(String),
    #[error("scaffold_attach_rejected: sandbox lifecycle {0:?}")]
    AttachRejected(Lifecycle),
    #[error("scaffold_scope_invalid: {0}")]
    InvalidScope(String),
    #[error("{0}")]
    Api(ApiErrorDisplay),
    #[error("device_join_grant_unavailable")]
    DeviceJoinGrantUnavailable,
    #[error("comet_not_installed_in_sandbox")]
    CometNotInstalled,
    /// The sandbox ran the host's launch as a generic exec rather than
    /// supervising it: its runtime doesn't serve this build's contract.
    #[error("scaffold_runtime_contract_mismatch: the sandbox ran the Crew host launch as a generic exec")]
    RuntimeContractMismatch,
}

impl From<reqwest::Error> for ScaffoldError {
    fn from(source: reqwest::Error) -> Self {
        Self::Transport {
            cause: classify_transport_error(&source),
            before_response: false,
            source: source.without_url(),
        }
    }
}

impl ScaffoldError {
    fn request_send_failure(source: reqwest::Error) -> Self {
        Self::Transport {
            cause: classify_transport_error(&source),
            before_response: true,
            source: source.without_url(),
        }
    }

    pub(crate) fn api_error(&self) -> Option<&ApiError> {
        match self {
            Self::Api(error) => Some(&error.0),
            _ => None,
        }
    }

    /// Platform (or the Edge) definitively refused it: a 4xx other than a
    /// timeout or a rate limit, or a refusal no observation can turn into
    /// success. Everything else (the network, 5xx, cancellation, an answer
    /// that wasn't understood) leaves the outcome unknown.
    pub(crate) fn is_rejection(&self) -> bool {
        matches!(self, Self::AttachRejected(_) | Self::RuntimeContractMismatch | Self::InvalidScope(_))
            || self
                .api_error()
                .is_some_and(|error| (400..500).contains(&error.status) && !matches!(error.status, 408 | 429))
    }

    fn is_runtime_starting(&self) -> bool {
        self.api_error()
            .is_some_and(|error| error.status == 503 && error.code == "sandbox_runtime_starting")
    }

    fn is_attach_probe_transport_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport {
                cause: TransportCause::RequestTimeout | TransportCause::ConnectionResetOrClosed,
                before_response: true,
                ..
            }
        )
    }
}

fn classify_transport_error(error: &reqwest::Error) -> TransportCause {
    if error.is_timeout() {
        return TransportCause::RequestTimeout;
    }
    let mut source: &(dyn std::error::Error + 'static) = error;
    loop {
        if let Some(io_error) = source.downcast_ref::<std::io::Error>()
            && matches!(
                io_error.kind(),
                std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            )
        {
            return TransportCause::ConnectionResetOrClosed;
        }
        if let Some(hyper_error) = source.downcast_ref::<hyper::Error>()
            && (hyper_error.is_closed() || hyper_error.is_incomplete_message())
        {
            return TransportCause::ConnectionResetOrClosed;
        }
        let Some(next) = source.source() else { break };
        source = next;
    }
    if error.is_connect() {
        TransportCause::ConnectFailure
    } else {
        TransportCause::SendFailure
    }
}

fn http_client(total_timeout: Duration) -> Result<reqwest::Client, ScaffoldError> {
    Ok(reqwest::Client::builder()
        .connect_timeout(REQUEST_TIMEOUT)
        .timeout(total_timeout)
        // A request carrying a bearer never follows a redirect anywhere.
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// An exact HTTPS origin (HTTP only on loopback).
fn exact_origin(value: &str) -> Result<Url, ScaffoldError> {
    let invalid = || ScaffoldError::InvalidScope("a Scaffold or Edge URL must be an exact HTTPS origin (HTTP only on loopback)".into());
    let mut url = Url::parse(value).map_err(|_| invalid())?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]" | "::1"));
    if url.host_str().is_none()
        || (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(invalid());
    }
    url.set_path("");
    Ok(url)
}

/// The bearer now, or why not.
async fn credential(source: &dyn TokenSource) -> Result<String, ScaffoldError> {
    match source.token().await {
        Ok(token) if !token.trim().is_empty() => Ok(token),
        Ok(_) | Err(TokenError::SignedOut) => Err(ScaffoldError::AuthUnavailable),
        Err(TokenError::TemporarilyUnavailable(_)) => Err(ScaffoldError::AuthTemporarilyUnavailable),
    }
}

async fn send_authenticated(
    source: &dyn TokenSource,
    request: reqwest::RequestBuilder,
    cancellation: &CancellationToken,
) -> Result<reqwest::Response, ScaffoldError> {
    let bearer = credential(source).await?;
    let send = request.bearer_auth(&bearer).send();
    drop(bearer);
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(ScaffoldError::Cancelled),
        response = send => response.map_err(ScaffoldError::request_send_failure),
    }
}

/// A bounded body, read while the request may still be cancelled.
async fn bounded_body(mut response: reqwest::Response, limit: usize, cancellation: &CancellationToken) -> Result<Vec<u8>, ScaffoldError> {
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ScaffoldError::Cancelled),
            chunk = response.chunk() => chunk?,
        };
        let Some(chunk) = chunk else { return Ok(bytes) };
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(ScaffoldError::InvalidResponse("the answer is too large".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
}

fn response_retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some((date.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().unwrap_or_default())
}

/// A refusal's status, code and message (Crew's `api_error`): the code from
/// `error` (a string, or its `error`/`code`), the provider's startup state
/// promoted only when exact; the message bounded and redacted.
fn api_error(status: StatusCode, body: &[u8], retry_after: Option<Duration>) -> ScaffoldError {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        error: Value,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        error_description: Option<String>,
        #[serde(default)]
        body: Value,
    }
    let decoded = serde_json::from_slice::<Envelope>(body).ok();
    let error_value = decoded.as_ref().map(|value| &value.error);
    let runtime_starting = status == StatusCode::SERVICE_UNAVAILABLE
        && error_value.and_then(Value::as_str) == Some("sandbox_provider_error")
        && decoded.as_ref().and_then(|value| value.body.get("error")).and_then(Value::as_str) == Some("sandbox_runtime_starting");
    let code = runtime_starting
        .then_some("sandbox_runtime_starting")
        .or_else(|| error_value.and_then(Value::as_str))
        .or_else(|| error_value.and_then(|value| value.get("error")).and_then(Value::as_str))
        .or_else(|| error_value.and_then(|value| value.get("code")).and_then(Value::as_str))
        .filter(|code| !code.is_empty() && code.len() <= 128 && code.bytes().all(|byte| byte.is_ascii_graphic()))
        .unwrap_or("scaffold_request_rejected")
        .to_string();
    let nested = error_value
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let message = decoded
        .and_then(|value| value.message.or(value.error_description))
        .or(nested)
        .map(|message| {
            let message: String = message.chars().filter(|c| !c.is_control()).take(240).collect();
            zeron_harness::redact::redact_output(&message)
        })
        .filter(|message| !message.trim().is_empty());
    ScaffoldError::Api(ApiErrorDisplay(ApiError {
        status: status.as_u16(),
        code,
        message,
        retry_after,
    }))
}

// -- Platform's sandboxes ----------------------------------------------------------

#[derive(Deserialize)]
struct SandboxListEnvelope {
    ok: bool,
    #[serde(default)]
    sandboxes: Vec<Value>,
}

#[derive(Deserialize)]
struct SandboxEnvelope {
    sandbox: Sandbox,
}

/// A sandbox as Platform answers it: only what Crew checks and keeps.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Sandbox {
    id: String,
    name: Option<String>,
    lifecycle_epoch: Option<u64>,
    status: Lifecycle,
    kind: Option<String>,
    runtime_profile: Option<String>,
    region: Option<String>,
    selected_region: Option<String>,
    source_ref: Option<String>,
    #[serde(default)]
    database_environment: DatabaseEnvironment,
    owner_email: Option<String>,
    created_at: String,
    updated_at: String,
    last_activity_at: Option<String>,
    #[serde(default)]
    links: serde_json::Map<String, Value>,
    comet_runtime_profile: Option<RuntimeProfile>,
    agent_route: Option<AgentRoute>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeProfile {
    version: String,
    project_id: String,
    deployment_id: String,
    session_id: String,
    sandbox_id: String,
}

fn parse_rfc3339_ms(value: &str, field: &str) -> Result<i64, ScaffoldError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_millis())
        .map_err(|_| ScaffoldError::InvalidResponse(format!("invalid {field}")))
}

impl Sandbox {
    /// Crew's response check: a Crew-compatible remote sandbox, a verified
    /// owner, valid times, and the authority profile of exactly `scope`.
    pub(crate) fn into_environment(self, scope: &ScaffoldScope) -> Result<SessionEnvironment, ScaffoldError> {
        if !scaffold_id(&self.id) {
            return Err(ScaffoldError::InvalidResponse("the sandbox id is invalid".into()));
        }
        if self.kind.as_deref().is_some_and(|kind| {
            kind != "remote_code" && !(kind == "agent" && self.runtime_profile.as_deref() == Some("comet_remote"))
        }) || self
            .runtime_profile
            .as_deref()
            .is_some_and(|profile| !matches!(profile, "remote_code" | "comet_remote"))
        {
            return Err(ScaffoldError::InvalidResponse(format!("{} is not a Crew-compatible remote sandbox", self.id)));
        }
        let owner = self
            .owner_email
            .as_deref()
            .map(str::trim)
            .filter(|owner| !owner.is_empty() && owner.contains('@'))
            .ok_or_else(|| ScaffoldError::InvalidResponse(format!("sandbox {} has no verified owner", self.id)))?
            .to_owned();
        let activity = self.last_activity_at.as_deref().unwrap_or(&self.updated_at);
        let last_activity_at = Some(parse_rfc3339_ms(activity, "lastActivityAt")?);
        parse_rfc3339_ms(&self.created_at, "createdAt")?;
        parse_rfc3339_ms(&self.updated_at, "updatedAt")?;
        let profile = self
            .comet_runtime_profile
            .as_ref()
            .ok_or_else(|| ScaffoldError::InvalidResponse(format!("sandbox {} has no Crew runtime profile", self.id)))?;
        if profile.version != PROFILE_VERSION
            || profile.project_id != scope.project_id
            || Some(profile.deployment_id.as_str()) != scope.deployment_id.as_deref()
            || Some(profile.session_id.as_str()) != scope.session_id.as_deref()
            || profile.sandbox_id != self.id
        {
            return Err(ScaffoldError::InvalidResponse(format!("sandbox {} returned a mismatched Crew runtime profile", self.id)));
        }
        if let Some(route) = &self.agent_route {
            route.validate().map_err(|why| ScaffoldError::InvalidResponse(why.into()))?;
        }
        Ok(SessionEnvironment {
            scope: ScaffoldScope {
                project_id: profile.project_id.clone(),
                deployment_id: Some(profile.deployment_id.clone()),
                session_id: Some(profile.session_id.clone()),
            },
            source: EnvironmentSource::Scaffold {
                sandbox_id: self.id,
                region: self.selected_region.or(self.region),
                lifecycle: self.status,
                lifecycle_epoch: self.lifecycle_epoch,
                links: self.links,
            },
            name: self.name,
            owner_principal: owner,
            source_ref: self.source_ref,
            last_activity_at,
            database_environment: Some(self.database_environment),
            agent_route: self.agent_route,
        })
    }

    /// The session its authority profile names, if it has one.
    fn session_id(value: &Value) -> Option<&str> {
        value.pointer("/cometRuntimeProfile/sessionId").and_then(Value::as_str)
    }
}

/// What a create may say besides its session and route.
#[derive(Clone, Copy, Default)]
pub(crate) struct CreateOptions<'a> {
    pub name: Option<&'a str>,
    pub source_ref: Option<&'a str>,
    pub region: Option<&'a str>,
    pub database_environment: DatabaseEnvironment,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<CreateSource<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<&'a str>,
    database_environment: DatabaseEnvironment,
    agent_route: &'a AgentRoute,
    comet_runtime_profile: CreateProfile<'a>,
}

#[derive(Debug, Serialize)]
struct CreateSource<'a> {
    #[serde(rename = "ref")]
    reference: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateProfile<'a> {
    version: &'static str,
    project_id: &'a str,
    deployment_id: &'a str,
    session_id: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecBody<'a> {
    argv: &'a [String],
    mode: &'static str,
    timeout_ms: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecResponse {
    ok: bool,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    supervised: bool,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Platform's sandboxes, as the engine's verified profile.
#[derive(Clone)]
pub(crate) struct ScaffoldClient {
    http: reqwest::Client,
    origin: Url,
    project_scope: String,
    bearer: Arc<dyn TokenSource>,
}

impl fmt::Debug for ScaffoldClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScaffoldClient")
            .field("origin", &self.origin.as_str())
            .field("project_scope", &self.project_scope)
            .field("bearer", &"<provider>")
            .finish()
    }
}

impl ScaffoldClient {
    pub(crate) fn new(origin: &str, project_scope: impl Into<String>, bearer: Arc<dyn TokenSource>) -> Result<Self, ScaffoldError> {
        let project_scope = project_scope.into();
        if project_scope.trim().is_empty() {
            return Err(ScaffoldError::InvalidScope("projectId is required".into()));
        }
        Ok(Self {
            http: http_client(REQUEST_TIMEOUT)?,
            origin: exact_origin(origin)?,
            project_scope,
            bearer,
        })
    }

    /// The authority's origin as its supervisor pins it: no trailing `/`.
    fn origin_text(&self) -> String {
        self.origin.as_str().trim_end_matches('/').to_owned()
    }

    /// Every sandbox of `scope`'s project and deployment (and session, when it
    /// names one) that passes the response checks. One that doesn't is left
    /// out, never trusted.
    pub(crate) async fn list(&self, scope: &ScaffoldScope, cancellation: &CancellationToken) -> Result<Vec<SessionEnvironment>, ScaffoldError> {
        let mut validation = scope.clone();
        validation.session_id.get_or_insert_with(|| "list".into());
        self.validate_scope(&validation)?;
        let listed: SandboxListEnvelope = self.request(Method::GET, self.sandboxes_url(), None::<&()>, cancellation).await?;
        if !listed.ok {
            return Err(ScaffoldError::InvalidResponse("the list reported ok=false".into()));
        }
        let mut environments = Vec::new();
        for sandbox in listed.sandboxes {
            let Some(session) = Sandbox::session_id(&sandbox).map(str::to_owned) else { continue };
            if scope.session_id.as_deref().is_some_and(|wanted| wanted != session) {
                continue;
            }
            let mut exact = scope.clone();
            exact.session_id = Some(session);
            let parsed = serde_json::from_value::<Sandbox>(sandbox)
                .map_err(|_| ScaffoldError::InvalidResponse("not a sandbox".into()))
                .and_then(|sandbox| sandbox.into_environment(&exact));
            match parsed {
                Ok(environment) => environments.push(environment),
                Err(error) => tracing::debug!(%error, "crew: a listed sandbox failed its checks; left out"),
            }
        }
        Ok(environments)
    }

    pub(crate) async fn inspect(&self, sandbox_id: &str, scope: &ScaffoldScope, cancellation: &CancellationToken) -> Result<SessionEnvironment, ScaffoldError> {
        self.validate_scope(scope)?;
        let answer: SandboxEnvelope = self.request(Method::GET, self.sandbox_url(sandbox_id, None)?, None::<&()>, cancellation).await?;
        answer.sandbox.into_environment(scope)
    }

    /// `POST /api/code-sandboxes`. Idempotent by session: Platform derives
    /// the sandbox from the authority profile, and answers the same sandbox
    /// (200) for the same session and route.
    pub(crate) async fn create(
        &self,
        scope: &ScaffoldScope,
        options: CreateOptions<'_>,
        agent_route: &AgentRoute,
        cancellation: &CancellationToken,
    ) -> Result<SessionEnvironment, ScaffoldError> {
        self.validate_scope(scope)?;
        agent_route.validate().map_err(|why| ScaffoldError::InvalidScope(why.into()))?;
        let body = CreateBody {
            name: options.name,
            source: options.source_ref.map(|reference| CreateSource { reference }),
            region: options.region,
            database_environment: options.database_environment,
            agent_route,
            comet_runtime_profile: CreateProfile {
                version: PROFILE_VERSION,
                project_id: &scope.project_id,
                deployment_id: scope.deployment_id.as_deref().expect("validated deploymentId"),
                session_id: scope.session_id.as_deref().expect("validated sessionId"),
            },
        };
        let answer: SandboxEnvelope = self.request(Method::POST, self.sandboxes_url(), Some(&body), cancellation).await?;
        answer.sandbox.into_environment(scope)
    }

    async fn exec(&self, sandbox_id: &str, body: &ExecBody<'_>, cancellation: &CancellationToken) -> Result<ExecResponse, ScaffoldError> {
        self.request(Method::POST, self.sandbox_url(sandbox_id, Some("exec"))?, Some(body), cancellation).await
    }

    async fn put_file(&self, sandbox_id: &str, path: &str, content: &str, cancellation: &CancellationToken) -> Result<(), ScaffoldError> {
        let mut url = self.sandbox_url(sandbox_id, Some("files"))?;
        url.query_pairs_mut().append_pair("path", path);
        let _: Value = self
            .request(Method::PUT, url, Some(&serde_json::json!({ "content": content })), cancellation)
            .await?;
        Ok(())
    }

    /// Best effort: whatever happens, the caller's outcome stands.
    async fn remove_file(&self, sandbox_id: &str, path: &str, cancellation: &CancellationToken) {
        let argv = ["rm", "-f", "--", path].map(String::from);
        let _ = self
            .exec(sandbox_id, &ExecBody { argv: &argv, mode: "inline", timeout_ms: 10_000 }, cancellation)
            .await;
    }

    async fn request<T, B>(&self, method: Method, url: Url, body: Option<&B>, cancellation: &CancellationToken) -> Result<T, ScaffoldError>
    where
        T: for<'de> Deserialize<'de>,
        B: Serialize + ?Sized,
    {
        let mut request = self.http.request(method, url).header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = send_authenticated(self.bearer.as_ref(), request, cancellation).await?;
        let status = response.status();
        let retry_after = response_retry_after(&response);
        let bytes = bounded_body(response, MAX_JSON_BYTES, cancellation).await?;
        if !status.is_success() {
            return Err(api_error(status, &bytes, retry_after));
        }
        serde_json::from_slice(&bytes).map_err(|_| ScaffoldError::InvalidResponse("not the expected JSON".into()))
    }

    fn validate_scope(&self, scope: &ScaffoldScope) -> Result<(), ScaffoldError> {
        if scope.project_id != self.project_scope {
            return Err(ScaffoldError::InvalidScope(format!("projectId must be {}", self.project_scope)));
        }
        if scope.deployment_id.as_deref().is_none_or(|id| !scaffold_id(id)) {
            return Err(ScaffoldError::InvalidScope("deploymentId is required".into()));
        }
        if scope.session_id.as_deref().is_none_or(|id| !scaffold_id(id)) {
            return Err(ScaffoldError::InvalidScope("sessionId is required".into()));
        }
        Ok(())
    }

    fn sandboxes_url(&self) -> Url {
        let mut url = self.origin.clone();
        url.path_segments_mut().expect("an HTTP origin").extend(["api", "code-sandboxes"]);
        url
    }

    fn sandbox_url(&self, sandbox_id: &str, action: Option<&str>) -> Result<Url, ScaffoldError> {
        if !scaffold_id(sandbox_id) {
            return Err(ScaffoldError::InvalidResponse("the sandbox id is invalid".into()));
        }
        let mut url = self.sandboxes_url();
        {
            let mut segments = url.path_segments_mut().expect("an HTTP origin");
            segments.push(sandbox_id);
            if let Some(action) = action {
                segments.push(action);
            }
        }
        Ok(url)
    }
}

// -- the Edge's device grants --------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct DeviceJoinGrantRequest {
    pub principal_subject: String,
    pub scope: ScaffoldScope,
    pub sandbox_id: String,
    pub device_id: String,
    pub lifecycle_epoch: u64,
    pub capabilities: Vec<String>,
    pub expires_in_seconds: u32,
}

/// A one-time device-join grant (`cg1.<id>.<secret>`). Never printed.
pub(crate) struct DeviceJoinCredential {
    credential: String,
    grant_id: String,
}

impl fmt::Debug for DeviceJoinCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceJoinCredential")
            .field("credential", &"<redacted>")
            .field("grant_id", &self.grant_id)
            .finish()
    }
}

#[async_trait]
pub(crate) trait DeviceJoinGrantProvider: Send + Sync {
    async fn mint(&self, request: &DeviceJoinGrantRequest, cancellation: &CancellationToken) -> Result<DeviceJoinCredential, ScaffoldError>;
}

/// `cg1.<32 hex>.<secret>`'s id.
fn device_join_grant_id(credential: &str) -> Option<String> {
    let mut parts = credential.split('.');
    let (prefix, id, secret) = (parts.next()?, parts.next()?, parts.next()?);
    (prefix == "cg1"
        && id.len() == 32
        && id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !secret.is_empty()
        && parts.next().is_none())
    .then(|| id.to_owned())
}

/// This engine's Edge's `POST /auth/device-grants`, as the owner.
pub(crate) struct EdgeDeviceJoinGrantClient {
    http: reqwest::Client,
    origin: Url,
    bearer: Arc<dyn TokenSource>,
}

impl EdgeDeviceJoinGrantClient {
    pub(crate) fn new(origin: &str, bearer: Arc<dyn TokenSource>) -> Result<Self, ScaffoldError> {
        Ok(Self {
            http: http_client(REQUEST_TIMEOUT)?,
            origin: exact_origin(origin)?,
            bearer,
        })
    }
}

#[async_trait]
impl DeviceJoinGrantProvider for EdgeDeviceJoinGrantClient {
    async fn mint(&self, request: &DeviceJoinGrantRequest, cancellation: &CancellationToken) -> Result<DeviceJoinCredential, ScaffoldError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Answer {
            grant: String,
            expires_at: i64,
            access_expires_at: Option<i64>,
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body<'a> {
            target_device_id: &'a str,
            session_id: &'a str,
            deployment_id: &'a str,
            sandbox_id: &'a str,
            lifecycle_epoch: u64,
            capabilities: &'a [String],
            ttl_seconds: u32,
        }
        let unavailable = || ScaffoldError::DeviceJoinGrantUnavailable;
        let session_id = request.scope.session_id.as_deref().filter(|id| !id.trim().is_empty()).ok_or_else(unavailable)?;
        let deployment_id = request.scope.deployment_id.as_deref().filter(|id| !id.trim().is_empty()).ok_or_else(unavailable)?;
        if request.sandbox_id.trim().is_empty() || request.principal_subject.trim().is_empty() {
            return Err(unavailable());
        }
        let body = Body {
            target_device_id: &request.device_id,
            session_id,
            deployment_id,
            sandbox_id: &request.sandbox_id,
            lifecycle_epoch: request.lifecycle_epoch,
            capabilities: &request.capabilities,
            ttl_seconds: request.expires_in_seconds,
        };
        let mut url = self.origin.clone();
        url.path_segments_mut().expect("an HTTP origin").extend(JOIN_GRANT_PATH);
        let http = self.http.post(url).header(reqwest::header::ACCEPT, "application/json").json(&body);
        let response = send_authenticated(self.bearer.as_ref(), http, cancellation)
            .await
            .map_err(|error| match error {
                ScaffoldError::Transport { .. } => unavailable(),
                error => error,
            })?;
        let status = response.status();
        if !status.is_success() {
            // The code only, never the body (it may echo a credential).
            let retry_after = response_retry_after(&response);
            let bytes = bounded_body(response, 16 * 1024, cancellation).await.unwrap_or_default();
            let code = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(Value::as_str)
                        .or_else(|| value.pointer("/error/code").and_then(Value::as_str))
                        .or_else(|| value.get("code").and_then(Value::as_str))
                        .map(str::to_owned)
                })
                .filter(|code| {
                    !code.is_empty() && code.len() <= 128 && code.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                })
                .unwrap_or_else(|| "device_join_grant_rejected".into());
            return Err(ScaffoldError::Api(ApiErrorDisplay(ApiError {
                status: status.as_u16(),
                code,
                message: None,
                retry_after,
            })));
        }
        let bytes = bounded_body(response, 16 * 1024, cancellation).await.map_err(|error| match error {
            ScaffoldError::Cancelled => ScaffoldError::Cancelled,
            _ => unavailable(),
        })?;
        let answer: Answer = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        let now = now_ms();
        let control_expires_at = answer.access_expires_at.unwrap_or_else(|| {
            answer
                .expires_at
                .saturating_sub(i64::from(request.expires_in_seconds) * 1000)
                .saturating_add(DEVICE_ACCESS_TTL_MS)
        });
        let grant_id = device_join_grant_id(&answer.grant)
            .filter(|_| answer.expires_at > now && control_expires_at > answer.expires_at)
            .ok_or_else(unavailable)?;
        Ok(DeviceJoinCredential {
            credential: answer.grant,
            grant_id,
        })
    }
}

// -- Attach --------------------------------------------------------------------

/// The desktop's half of a sandbox host: Platform's sandboxes, this engine's
/// Edge, and its device grants.
#[derive(Clone)]
pub(crate) struct SandboxHosts {
    client: ScaffoldClient,
    /// This engine's own Edge: the host's `--edge-url`.
    edge_origin: String,
    grants: Arc<dyn DeviceJoinGrantProvider>,
}

impl SandboxHosts {
    pub(crate) fn new(client: ScaffoldClient, edge_origin: &str, grants: Arc<dyn DeviceJoinGrantProvider>) -> Result<Self, ScaffoldError> {
        Ok(Self {
            client,
            edge_origin: exact_origin(edge_origin)?.as_str().trim_end_matches('/').to_owned(),
            grants,
        })
    }

    pub(crate) fn client(&self) -> &ScaffoldClient {
        &self.client
    }

    /// The host's own command line: `comet --data-dir .scaffold/crew-data
    /// --ipc-port 39400 <command>`.
    fn remote_argv(command: &str) -> Vec<String> {
        [HOST_EXECUTABLE, "--data-dir", HOST_DATA_DIR, "--ipc-port", HOST_IPC_PORT, command]
            .map(String::from)
            .to_vec()
    }

    /// Attach `sandbox_id`'s host for `scope` (Crew's `control(Attach)` less
    /// resume): reuse an exactly bound host, else mint, hand over and start
    /// one, and wait for its probe to confirm it.
    pub(crate) async fn attach(
        &self,
        sandbox_id: &str,
        scope: &ScaffoldScope,
        cancellation: &CancellationToken,
    ) -> Result<(SessionEnvironment, Attachment), ScaffoldError> {
        let environment = self.client.inspect(sandbox_id, scope, cancellation).await?;
        let (_, lifecycle, _) = environment.sandbox()?;
        // (Crew resumes a paused sandbox here; this port doesn't yet.)
        if !lifecycle.attachable() {
            return Err(ScaffoldError::AttachRejected(lifecycle));
        }
        let attachment = self.start_host(sandbox_id, scope, &environment, cancellation).await?;
        Ok((environment, attachment))
    }

    /// A host already serving `environment`, observed: no grant minted, no
    /// file written, nothing started.
    pub(crate) async fn observe(&self, environment: &SessionEnvironment, cancellation: &CancellationToken) -> Result<Option<Attachment>, ScaffoldError> {
        let (sandbox_id, lifecycle, _) = environment.sandbox()?;
        if !lifecycle.attachable() {
            return Ok(None);
        }
        let response = self
            .probe(sandbox_id, &environment.scope, environment, cancellation)
            .await?;
        Ok(confirmed(&response, environment).map(|authority| Attachment {
            device_id: authority.device_id,
            run_id: None,
            grant: ControlGrant {
                id: authority.grant_id,
                expires_at: authority.expires_at,
                capabilities: authority.capabilities,
            },
        }))
    }

    /// `scaffold-authority`, inline: only a read, so it alone is retried, and
    /// only while Platform says this same epoch's runtime is starting (or a
    /// bounded number of times, for a transport failure before any answer).
    async fn probe(
        &self,
        sandbox_id: &str,
        scope: &ScaffoldScope,
        environment: &SessionEnvironment,
        cancellation: &CancellationToken,
    ) -> Result<ExecResponse, ScaffoldError> {
        let argv = Self::remote_argv("scaffold-authority");
        let mut transport_retries = 0;
        loop {
            let body = ExecBody { argv: &argv, mode: "inline", timeout_ms: 10_000 };
            match self.client.exec(sandbox_id, &body, cancellation).await {
                Err(error) if error.is_runtime_starting() => {}
                Err(error) if error.is_attach_probe_transport_retryable() && transport_retries < ATTACH_PROBE_TRANSPORT_RETRIES => {
                    transport_retries += 1;
                }
                result => return result,
            }
            // Only this same lifecycle starting extends the wait.
            let pending = async {
                let current = self.client.inspect(sandbox_id, scope, cancellation).await?;
                let (current_id, lifecycle, epoch) = current.sandbox()?;
                if current_id != sandbox_id
                    || current.scope != environment.scope
                    || current.owner_principal != environment.owner_principal
                    || epoch != environment.sandbox()?.2
                    || !lifecycle.attachable()
                {
                    return Err(ScaffoldError::InvalidResponse("the sandbox's lifecycle changed while its runtime started".into()));
                }
                tokio::time::sleep(RUNTIME_START_POLL).await;
                Ok(())
            };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ScaffoldError::Cancelled),
                result = pending => result?,
            }
        }
    }

    async fn start_host(
        &self,
        sandbox_id: &str,
        scope: &ScaffoldScope,
        environment: &SessionEnvironment,
        cancellation: &CancellationToken,
    ) -> Result<Attachment, ScaffoldError> {
        let deployment_id = scope
            .deployment_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| ScaffoldError::InvalidScope("deploymentId is required to attach".into()))?;
        let session_id = scope
            .session_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| ScaffoldError::InvalidScope("sessionId is required to attach".into()))?;
        let lifecycle_epoch = environment
            .sandbox()?
            .2
            .ok_or_else(|| ScaffoldError::InvalidResponse("a lifecycle epoch is required to attach".into()))?;

        // Platform owns installing and admitting the runtime. Reuse only an
        // exactly bound live host; else one background exec enters the native
        // runner's verified admission.
        let probed = self.probe(sandbox_id, scope, environment, cancellation).await?;
        if let Some(existing) = confirmed(&probed, environment) {
            return Ok(Attachment {
                device_id: existing.device_id,
                run_id: None,
                grant: ControlGrant {
                    id: existing.grant_id,
                    expires_at: existing.expires_at,
                    capabilities: existing.capabilities,
                },
            });
        }

        let device_id = device_id_for(sandbox_id, lifecycle_epoch);
        let request = DeviceJoinGrantRequest {
            principal_subject: environment.owner_principal.clone(),
            scope: scope.clone(),
            sandbox_id: sandbox_id.to_owned(),
            device_id: device_id.clone(),
            lifecycle_epoch,
            capabilities: host_capabilities(),
            expires_in_seconds: JOIN_GRANT_TTL_SECONDS,
        };
        let join = self.grants.mint(&request, cancellation).await?;

        // The one-time grant goes in a private file, never argv; the host
        // consumes it once the grant is its verified authority.
        let bootstrap_path = format!(".comet-device-bootstrap-{}.json", uuid::Uuid::new_v4());
        let bootstrap = serde_json::json!({
            "deviceJoinGrant": join.credential,
            "projectId": scope.project_id,
            "deploymentId": deployment_id,
            "sessionId": session_id,
            "deviceId": device_id,
            "lifecycleEpoch": lifecycle_epoch,
            "sandboxId": sandbox_id,
        })
        .to_string();
        drop(join);
        let result = async {
            self.client.put_file(sandbox_id, &bootstrap_path, &bootstrap, cancellation).await?;
            let chmod = ["chmod", "600", bootstrap_path.as_str()].map(String::from);
            let secured = self
                .client
                .exec(sandbox_id, &ExecBody { argv: &chmod, mode: "inline", timeout_ms: 10_000 }, cancellation)
                .await?;
            if !secured.ok || secured.exit_code != Some(0) {
                return Err(ScaffoldError::InvalidResponse("could not secure the device bootstrap file".into()));
            }
            let mut argv = Self::remote_argv("headless");
            argv.extend([
                "--device-bootstrap-file".to_owned(),
                bootstrap_path.clone(),
                "--edge-url".to_owned(),
                self.edge_origin.clone(),
                "--scaffold-url".to_owned(),
                // The supervisor pins the authority exactly: no implicit `/`.
                self.client.origin_text(),
                "--project-scope".to_owned(),
                scope.project_id.clone(),
                "--deployment-id".to_owned(),
                deployment_id.to_owned(),
            ]);
            let started = self
                .client
                .exec(sandbox_id, &ExecBody { argv: &argv, mode: "background", timeout_ms: 0 }, cancellation)
                .await?;
            if !started.ok {
                if started.error.as_deref() == Some("exec_spawn_failed") {
                    return Err(ScaffoldError::CometNotInstalled);
                }
                return Err(ScaffoldError::InvalidResponse(
                    started.error.unwrap_or_else(|| "the sandbox refused the launch".into()),
                ));
            }
            // A current supervisor marks every native launch it admits; an
            // unmarked success is the generic exec path.
            if !started.supervised {
                return Err(ScaffoldError::RuntimeContractMismatch);
            }
            let Some(run_id) = started.run_id.filter(|id| !id.trim().is_empty()) else {
                return Err(ScaffoldError::InvalidResponse("the launch returned no supervised runId".into()));
            };
            // The runner may keep a host it already supervises: the grant
            // just minted proves nothing until the probe says so.
            let wait = async {
                loop {
                    let authority = self.probe(sandbox_id, scope, environment, cancellation).await?;
                    if let Some(confirmed) = confirmed(&authority, environment) {
                        return Ok(confirmed);
                    }
                    // A completed answer that doesn't match is final.
                    if authority.ok && authority.exit_code == Some(0) {
                        return Err(ScaffoldError::InvalidResponse("the host answered another authority".into()));
                    }
                    tokio::time::sleep(RUNTIME_START_POLL).await;
                }
            };
            let confirmed = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ScaffoldError::Cancelled),
                result = tokio::time::timeout(REQUEST_TIMEOUT, wait) => result.map_err(|_| {
                    ScaffoldError::InvalidResponse("the host did not confirm its authority within 30 seconds".into())
                })??,
            };
            Ok(Attachment {
                device_id: device_id.clone(),
                run_id: Some(run_id),
                grant: ControlGrant {
                    id: confirmed.grant_id,
                    expires_at: confirmed.expires_at,
                    capabilities: confirmed.capabilities,
                },
            })
        }
        .await;
        // A failed upload may still have landed: remove this attempt's file
        // whatever happened, on its own bounded budget, never replacing the
        // outcome.
        let cleanup = CancellationToken::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), self.client.remove_file(sandbox_id, &bootstrap_path, &cleanup)).await;
        result
    }
}

/// The probe's answer, if it is exactly this environment's host: an
/// unexpired grant for its owner, scope, sandbox, device, epoch and Crew's
/// capabilities (Crew's `confirmed_host_authority`).
fn confirmed(response: &ExecResponse, environment: &SessionEnvironment) -> Option<DeviceGrantAuthority> {
    if !response.ok || response.exit_code != Some(0) {
        return None;
    }
    let EnvironmentSource::Scaffold {
        sandbox_id,
        lifecycle_epoch: Some(epoch),
        ..
    } = &environment.source
    else {
        return None;
    };
    let authority: DeviceGrantAuthority = serde_json::from_str(response.stdout.as_deref()?.trim()).ok()?;
    (!authority.grant_id.is_empty()
        && authority.expires_at > now_ms()
        && authority.principal_subject == environment.owner_principal
        && authority.scope == environment.scope
        && &authority.sandbox_id == sandbox_id
        && authority.device_id == device_id_for(sandbox_id, *epoch)
        && authority.lifecycle_epoch == *epoch
        && authority.capabilities == host_capabilities())
    .then_some(authority)
}

#[cfg(test)]
#[path = "crew_scaffold_tests.rs"]
pub(crate) mod tests;
