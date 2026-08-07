//! Native Scaffold code-sandbox control plane and Comet bootstrap.
//!
//! The long-lived Scaffold OAuth bearer is consulted per request and is used only
//! as the HTTP `Authorization` header. It is never included in sandbox exec input,
//! collaboration records, watch snapshots, errors, or logs. A sandbox receives only
//! a short-lived Comet device-join credential minted by the edge.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use comet_harness::CancellationToken;
use comet_proto::{
    CAPABILITY_SESSION_ANNOTATE, CAPABILITY_SESSION_CHAT, CAPABILITY_SESSION_CONTROL,
    CAPABILITY_SESSION_ENVIRONMENT, CAPABILITY_SESSION_FILES, CAPABILITY_SESSION_READ,
    CollaborationScope, OmpSessionArtifact, ScaffoldControlGrant, ScaffoldEnvironmentControl,
    ScaffoldEnvironmentControlResult, ScaffoldEnvironmentLinks, ScaffoldEnvironmentSnapshot,
    ScaffoldRuntimeMode, SessionEnvironment, SessionEnvironmentSource, SessionRoomProjection,
};
use comet_rpc::TokenSource;
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use crate::now_ms;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const JOIN_GRANT_TTL_SECONDS: u32 = 15 * 60;
const DEVICE_ACCESS_TTL_MS: i64 = 12 * 60 * 60 * 1000;
const JOIN_GRANT_PATH: [&str; 2] = ["auth", "device-grants"];

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaffoldApiError {
    pub status: u16,
    pub code: String,
    pub message: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ScaffoldError {
    #[error("scaffold_auth_unavailable")]
    AuthUnavailable,
    #[error("scaffold_request_cancelled")]
    Cancelled,
    #[error("scaffold_request_failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("scaffold_response_invalid: {0}")]
    InvalidResponse(String),
    #[error("scaffold_scope_invalid: {0}")]
    InvalidScope(String),
    #[error("{0}")]
    Api(ScaffoldApiErrorDisplay),
    #[error("device_join_grant_unavailable")]
    DeviceJoinGrantUnavailable,
    #[error("comet_not_installed_in_sandbox")]
    CometNotInstalled,
    #[error("omp_session_handoff_failed")]
    OmpSessionHandoffFailed,
}

/// Separate display wrapper prevents an upstream body from accidentally entering
/// logs while preserving the exact status/code/message as typed fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaffoldApiErrorDisplay(pub ScaffoldApiError);

impl fmt::Display for ScaffoldApiErrorDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "scaffold_api_error:{}:{}", self.0.status, self.0.code)?;
        if let Some(message) = &self.0.message {
            write!(f, ":{message}")?;
        }
        Ok(())
    }
}

impl ScaffoldError {
    pub fn api_error(&self) -> Option<&ScaffoldApiError> {
        match self {
            Self::Api(error) => Some(&error.0),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct ScaffoldClient {
    http: reqwest::Client,
    origin: Url,
    project_scope: String,
    bearer: Arc<dyn TokenSource>,
}

impl fmt::Debug for ScaffoldClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScaffoldClient")
            .field("origin", &self.origin)
            .field("project_scope", &self.project_scope)
            .field("bearer", &"<provider>")
            .finish()
    }
}

impl ScaffoldClient {
    pub fn new(
        origin: impl AsRef<str>,
        project_scope: impl Into<String>,
        bearer: Arc<dyn TokenSource>,
    ) -> Result<Self, ScaffoldError> {
        let origin = exact_origin(origin.as_ref())?;
        let project_scope = project_scope.into();
        if project_scope.trim().is_empty() {
            return Err(ScaffoldError::InvalidScope("projectId is required".into()));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(REQUEST_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // Upload grants are one-use credentials. Never follow a redirect
            // that could carry their Authorization header to another origin.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            origin,
            project_scope,
            bearer,
        })
    }

    pub fn project_scope(&self) -> &str {
        &self.project_scope
    }

    pub async fn list(
        &self,
        scope: &CollaborationScope,
        cancellation: &CancellationToken,
    ) -> Result<Vec<SessionEnvironment>, ScaffoldError> {
        self.validate_scope(scope)?;
        let response: SandboxListEnvelope = self
            .request(
                Method::GET,
                self.code_sandboxes_url(),
                Option::<&()>::None,
                cancellation,
            )
            .await?;
        if !response.ok {
            return Err(ScaffoldError::InvalidResponse(
                "list response reported ok=false without an HTTP error".into(),
            ));
        }
        response
            .sandboxes
            .into_iter()
            .map(|sandbox| sandbox.into_environment(scope.clone()))
            .collect()
    }

    pub async fn inspect(
        &self,
        sandbox_id: &str,
        scope: &CollaborationScope,
        cancellation: &CancellationToken,
    ) -> Result<SessionEnvironment, ScaffoldError> {
        self.validate_scope(scope)?;
        let response: SandboxEnvelope = self
            .request(
                Method::GET,
                self.sandbox_url(sandbox_id, None)?,
                Option::<&()>::None,
                cancellation,
            )
            .await?;
        response.sandbox.into_environment(scope.clone())
    }

    pub async fn create(
        &self,
        scope: &CollaborationScope,
        name: Option<&str>,
        source_ref: Option<&str>,
        region: Option<&str>,
        runtime_mode: Option<ScaffoldRuntimeMode>,
        cancellation: &CancellationToken,
    ) -> Result<SessionEnvironment, ScaffoldError> {
        self.validate_scope(scope)?;
        let deployment_id = scope
            .deployment_id
            .as_deref()
            .expect("validated deploymentId");
        let session_id = scope.session_id.as_deref().expect("validated sessionId");
        let body = CreateSandboxBody {
            name,
            source: source_ref.map(|reference| CreateSandboxSource { reference }),
            region,
            runtime_mode,
            comet_runtime_profile: CreateCometRuntimeProfile {
                version: "scaffold.comet-runtime.v1",
                project_id: &scope.project_id,
                deployment_id,
                session_id,
            },
        };
        let response: SandboxEnvelope = self
            .request(
                Method::POST,
                self.code_sandboxes_url(),
                Some(&body),
                cancellation,
            )
            .await?;
        response.sandbox.into_environment(scope.clone())
    }

    pub async fn pause(
        &self,
        sandbox_id: &str,
        scope: &CollaborationScope,
        cancellation: &CancellationToken,
    ) -> Result<SessionEnvironment, ScaffoldError> {
        self.lifecycle(sandbox_id, "pause", scope, cancellation)
            .await
    }

    pub async fn resume(
        &self,
        sandbox_id: &str,
        scope: &CollaborationScope,
        cancellation: &CancellationToken,
    ) -> Result<SessionEnvironment, ScaffoldError> {
        self.lifecycle(sandbox_id, "resume", scope, cancellation)
            .await
    }

    pub async fn stop(
        &self,
        sandbox_id: &str,
        scope: &CollaborationScope,
        cancellation: &CancellationToken,
    ) -> Result<SessionEnvironment, ScaffoldError> {
        self.lifecycle(sandbox_id, "stop", scope, cancellation)
            .await
    }

    async fn lifecycle(
        &self,
        sandbox_id: &str,
        action: &str,
        scope: &CollaborationScope,
        cancellation: &CancellationToken,
    ) -> Result<SessionEnvironment, ScaffoldError> {
        self.validate_scope(scope)?;
        let response: SandboxEnvelope = self
            .request(
                Method::POST,
                self.sandbox_url(sandbox_id, Some(action))?,
                Some(&EmptyBody {}),
                cancellation,
            )
            .await?;
        response.sandbox.into_environment(scope.clone())
    }

    async fn handoff_omp_session(
        &self,
        sandbox_id: &str,
        artifact: &OmpSessionArtifact,
        cancellation: &CancellationToken,
    ) -> Result<(), ScaffoldError> {
        validate_handoff_artifact(artifact)?;
        // Scaffold owns the absolute destination selection and must constrain it
        // below PI_CODING_AGENT_DIR. The archive itself contains only the
        // already-validated path relative to OMP's sessions directory.
        let grant: UploadGrantEnvelope = self
            .request(
                Method::POST,
                self.sandbox_url(sandbox_id, Some("uploads"))?,
                Some(&UploadGrantRequest {
                    destination_path: ".scaffold/omp-handoff-staging",
                }),
                cancellation,
            )
            .await?;
        if !grant.ok {
            return Err(ScaffoldError::OmpSessionHandoffFailed);
        }
        let grant = grant.upload;
        validate_upload_grant(&grant)?;
        let archive = single_file_tar(&artifact.storage_relative_path, &artifact.bytes, 0o600)?;
        self.upload_granted_archive(&grant, archive, cancellation)
            .await?;

        // Do not start OMP ACP here: that would create a second writer. The
        // ordinary first remote RunRequest uses the returned native id and
        // reaches ACP session/load in the harness adapter.
        let verify_argv = vec![
            "python3".to_string(),
            "-c".to_string(),
            VERIFY_HANDOFF_PYTHON.to_string(),
            grant.destination_path.clone(),
            artifact.storage_relative_path.clone(),
            artifact.sha256.clone(),
            artifact.byte_count.to_string(),
        ];
        let verified = self
            .exec(
                sandbox_id,
                &ExecBody {
                    argv: &verify_argv,
                    mode: "inline",
                    timeout_ms: 30_000,
                },
                cancellation,
            )
            .await?;
        if !verified.ok
            || verified.exit_code != Some(0)
            || verified.stdout.as_deref().map(str::trim) != Some("verified")
        {
            return Err(ScaffoldError::OmpSessionHandoffFailed);
        }
        Ok(())
    }

    async fn upload_granted_archive(
        &self,
        grant: &UploadGrant,
        archive: Vec<u8>,
        cancellation: &CancellationToken,
    ) -> Result<(), ScaffoldError> {
        let url = safe_upload_url(&grant.url)?;
        let token = grant.token.clone();
        let send = self
            .http
            .post(url)
            .bearer_auth(&token)
            .header(reqwest::header::CONTENT_TYPE, "application/x-tar")
            .body(archive)
            .send();
        tokio::pin!(send);
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ScaffoldError::Cancelled),
            response = &mut send => response?,
        };
        drop(token);
        if !response.status().is_success() {
            // Do not decode or retain an upload-service body: it may reflect a
            // one-use credential. Keep the failure deliberately opaque.
            return Err(ScaffoldError::OmpSessionHandoffFailed);
        }
        Ok(())
    }

    async fn exec(
        &self,
        sandbox_id: &str,
        body: &ExecBody<'_>,
        cancellation: &CancellationToken,
    ) -> Result<ExecResponse, ScaffoldError> {
        self.request(
            Method::POST,
            self.sandbox_url(sandbox_id, Some("exec"))?,
            Some(body),
            cancellation,
        )
        .await
    }

    async fn put_file(
        &self,
        sandbox_id: &str,
        path: &str,
        content: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), ScaffoldError> {
        let mut url = self.sandbox_url(sandbox_id, Some("files"))?;
        url.query_pairs_mut().append_pair("path", path);
        let _: Value = self
            .request(
                Method::PUT,
                url,
                Some(&serde_json::json!({ "content": content })),
                cancellation,
            )
            .await?;
        Ok(())
    }

    async fn remove_file(&self, sandbox_id: &str, path: &str, cancellation: &CancellationToken) {
        let argv = vec![
            "rm".to_string(),
            "-f".to_string(),
            "--".to_string(),
            path.to_string(),
        ];
        let _ = self
            .exec(
                sandbox_id,
                &ExecBody {
                    argv: &argv,
                    mode: "inline",
                    timeout_ms: 10_000,
                },
                cancellation,
            )
            .await;
    }

    async fn request<T, B>(
        &self,
        method: Method,
        url: Url,
        body: Option<&B>,
        cancellation: &CancellationToken,
    ) -> Result<T, ScaffoldError>
    where
        T: for<'de> Deserialize<'de>,
        B: Serialize + ?Sized,
    {
        let bearer = self
            .bearer
            .token()
            .await
            .filter(|token| !token.trim().is_empty())
            .ok_or(ScaffoldError::AuthUnavailable)?;
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(&bearer)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        let send = request.send();
        tokio::pin!(send);
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ScaffoldError::Cancelled),
            response = &mut send => response?,
        };
        // The bearer is dropped before any response body is retained or decoded.
        drop(bearer);
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(api_error(status, &bytes));
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| ScaffoldError::InvalidResponse(error.to_string()))
    }

    fn validate_scope(&self, scope: &CollaborationScope) -> Result<(), ScaffoldError> {
        if scope.project_id != self.project_scope {
            return Err(ScaffoldError::InvalidScope(format!(
                "projectId must equal configured Scaffold scope {}",
                self.project_scope
            )));
        }
        if scope
            .deployment_id
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(ScaffoldError::InvalidScope(
                "deploymentId is required".into(),
            ));
        }
        if scope
            .session_id
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(ScaffoldError::InvalidScope("sessionId is required".into()));
        }
        Ok(())
    }

    fn code_sandboxes_url(&self) -> Url {
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .expect("validated HTTP origin")
            .extend(["api", "code-sandboxes"]);
        url
    }

    fn sandbox_url(&self, sandbox_id: &str, action: Option<&str>) -> Result<Url, ScaffoldError> {
        if sandbox_id.trim().is_empty() {
            return Err(ScaffoldError::InvalidResponse(
                "sandboxId is required".into(),
            ));
        }
        let mut url = self.code_sandboxes_url();
        let mut segments = url.path_segments_mut().expect("validated HTTP origin");
        segments.push(sandbox_id);
        if let Some(action) = action {
            segments.push(action);
        }
        drop(segments);
        Ok(url)
    }
}

fn validate_handoff_artifact(artifact: &OmpSessionArtifact) -> Result<(), ScaffoldError> {
    use sha2::{Digest as _, Sha256};
    if artifact.native_session_id.trim().is_empty()
        || artifact.cwd.trim().is_empty()
        || artifact.byte_count != artifact.bytes.len() as u64
        || artifact.sha256.len() != 64
        || !artifact
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || format!("{:x}", Sha256::digest(&artifact.bytes)) != artifact.sha256
    {
        return Err(ScaffoldError::OmpSessionHandoffFailed);
    }
    safe_archive_path(&artifact.storage_relative_path)?;
    Ok(())
}

fn safe_archive_path(path: &str) -> Result<Vec<&str>, ScaffoldError> {
    let parts: Vec<_> = path.split('/').collect();
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || parts
            .iter()
            .any(|part| part.is_empty() || matches!(*part, "." | ".."))
    {
        return Err(ScaffoldError::OmpSessionHandoffFailed);
    }
    Ok(parts)
}

fn validate_upload_grant(grant: &UploadGrant) -> Result<(), ScaffoldError> {
    if grant.token.trim().is_empty()
        || grant.token_env != "SCAFFOLD_UPLOAD_TOKEN"
        || grant.destination_path.trim().is_empty()
        || grant.expires_at <= now_ms()
    {
        return Err(ScaffoldError::OmpSessionHandoffFailed);
    }
    safe_upload_url(&grant.url)?;
    Ok(())
}

fn safe_upload_url(value: &str) -> Result<Url, ScaffoldError> {
    let url = Url::parse(value).map_err(|_| ScaffoldError::OmpSessionHandoffFailed)?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
    if url.host_str().is_none()
        || (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || url.username() != ""
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ScaffoldError::OmpSessionHandoffFailed);
    }
    Ok(url)
}

/// Produce the minimal POSIX ustar archive accepted by Scaffold's one-use
/// upload endpoint, without spawning tar or writing artifact bytes to disk.
fn single_file_tar(path: &str, bytes: &[u8], mode: u32) -> Result<Vec<u8>, ScaffoldError> {
    let _ = safe_archive_path(path)?;
    let (name, prefix) = split_ustar_path(path)?;
    let mut header = [0_u8; 512];
    put_tar_text(&mut header[0..100], name)?;
    put_tar_octal(&mut header[100..108], mode as u64)?;
    put_tar_octal(&mut header[108..116], 0)?;
    put_tar_octal(&mut header[116..124], 0)?;
    put_tar_octal(&mut header[124..136], bytes.len() as u64)?;
    put_tar_octal(&mut header[136..148], 0)?;
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    put_tar_text(&mut header[345..500], prefix)?;
    let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    let checksum_field = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(checksum_field.as_bytes());

    let padded = bytes.len().div_ceil(512) * 512;
    let mut archive = Vec::with_capacity(512 + padded + 1024);
    archive.extend_from_slice(&header);
    archive.extend_from_slice(bytes);
    archive.resize(512 + padded, 0);
    archive.resize(512 + padded + 1024, 0);
    Ok(archive)
}

fn split_ustar_path(path: &str) -> Result<(&str, &str), ScaffoldError> {
    if path.len() <= 100 {
        return Ok((path, ""));
    }
    for (index, _) in path.match_indices('/').rev() {
        let (prefix, rest) = path.split_at(index);
        let name = &rest[1..];
        if prefix.len() <= 155 && !name.is_empty() && name.len() <= 100 {
            return Ok((name, prefix));
        }
    }
    Err(ScaffoldError::OmpSessionHandoffFailed)
}

fn put_tar_text(field: &mut [u8], value: &str) -> Result<(), ScaffoldError> {
    if value.len() > field.len() || !value.is_ascii() {
        return Err(ScaffoldError::OmpSessionHandoffFailed);
    }
    field[..value.len()].copy_from_slice(value.as_bytes());
    Ok(())
}

fn put_tar_octal(field: &mut [u8], value: u64) -> Result<(), ScaffoldError> {
    let digits = field.len() - 1;
    let value = format!("{value:0digits$o}");
    if value.len() != digits {
        return Err(ScaffoldError::OmpSessionHandoffFailed);
    }
    field[..digits].copy_from_slice(value.as_bytes());
    Ok(())
}

fn exact_origin(value: &str) -> Result<Url, ScaffoldError> {
    let mut url = Url::parse(value).map_err(|error| {
        ScaffoldError::InvalidResponse(format!("invalid Scaffold origin: {error}"))
    })?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
    if url.host_str().is_none()
        || (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(ScaffoldError::InvalidResponse(
            "Scaffold URL must be an exact HTTPS origin (HTTP is loopback-only)".into(),
        ));
    }
    url.set_path("");
    Ok(url)
}

fn api_error(status: StatusCode, body: &[u8]) -> ScaffoldError {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        error: Value,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        error_description: Option<String>,
    }

    let decoded = serde_json::from_slice::<Envelope>(body).ok();
    let error_value = decoded.as_ref().map(|value| &value.error);
    let code = error_value
        .and_then(Value::as_str)
        .or_else(|| {
            error_value
                .and_then(|value| value.get("error"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            error_value
                .and_then(|value| value.get("code"))
                .and_then(Value::as_str)
        })
        .unwrap_or("scaffold_request_rejected")
        .to_string();
    let nested_message = error_value
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let message = decoded
        .and_then(|value| value.message.or(value.error_description))
        .or(nested_message);
    ScaffoldError::Api(ScaffoldApiErrorDisplay(ScaffoldApiError {
        status: status.as_u16(),
        code,
        message,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SandboxListEnvelope {
    ok: bool,
    #[serde(default)]
    sandboxes: Vec<ScaffoldSandbox>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SandboxEnvelope {
    sandbox: ScaffoldSandbox,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScaffoldSandbox {
    id: String,
    lifecycle_epoch: Option<u64>,
    status: comet_proto::ScaffoldLifecycle,
    kind: Option<String>,
    runtime_profile: Option<String>,
    region: Option<String>,
    selected_region: Option<String>,
    owner_email: Option<String>,
    created_at: String,
    updated_at: String,
    last_activity_at: Option<String>,
    #[serde(default)]
    links: ScaffoldEnvironmentLinks,
}

impl ScaffoldSandbox {
    fn into_environment(
        self,
        scope: CollaborationScope,
    ) -> Result<SessionEnvironment, ScaffoldError> {
        if self.id.trim().is_empty() {
            return Err(ScaffoldError::InvalidResponse("sandbox id is empty".into()));
        }
        if self
            .kind
            .as_deref()
            .is_some_and(|kind| kind != "remote_code")
            || self
                .runtime_profile
                .as_deref()
                .is_some_and(|profile| !matches!(profile, "remote_code" | "comet_remote"))
        {
            return Err(ScaffoldError::InvalidResponse(format!(
                "{} is not a Comet-compatible remote sandbox",
                self.id
            )));
        }
        let owner_email = self.owner_email.ok_or_else(|| {
            ScaffoldError::InvalidResponse(format!("sandbox {} has no verified owner", self.id))
        })?;
        let owner_email = owner_email.trim();
        if owner_email.is_empty() || !owner_email.contains('@') {
            return Err(ScaffoldError::InvalidResponse(format!(
                "sandbox {} has an invalid verified owner",
                self.id
            )));
        }
        let owner_principal = format!("accounts.google.com:{owner_email}");
        let activity = self.last_activity_at.as_deref().unwrap_or(&self.updated_at);
        let last_activity_at = Some(parse_rfc3339_ms(activity, "lastActivityAt")?);
        // Validate required timestamps even though only last activity enters the projection.
        let _ = parse_rfc3339_ms(&self.created_at, "createdAt")?;
        let _ = parse_rfc3339_ms(&self.updated_at, "updatedAt")?;
        let _ = self.lifecycle_epoch;
        Ok(SessionEnvironment {
            source: SessionEnvironmentSource::Scaffold {
                sandbox_id: self.id,
                region: self.selected_region.or(self.region),
                lifecycle: self.status,
                links: self.links,
            },
            owner_principal,
            scope,
            last_activity_at,
            unknown: Default::default(),
        })
    }
}

fn parse_rfc3339_ms(value: &str, field: &str) -> Result<i64, ScaffoldError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_millis())
        .map_err(|error| ScaffoldError::InvalidResponse(format!("invalid {field}: {error}")))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateSandboxBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<CreateSandboxSource<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime_mode: Option<ScaffoldRuntimeMode>,
    comet_runtime_profile: CreateCometRuntimeProfile<'a>,
}

#[derive(Debug, Serialize)]
struct CreateSandboxSource<'a> {
    #[serde(rename = "ref")]
    reference: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateCometRuntimeProfile<'a> {
    version: &'static str,
    project_id: &'a str,
    deployment_id: &'a str,
    session_id: &'a str,
}

#[derive(Debug, Serialize)]
struct EmptyBody {}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecBody<'a> {
    argv: &'a [String],
    mode: &'static str,
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecResponse {
    ok: bool,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadGrantRequest<'a> {
    destination_path: &'a str,
}

#[derive(Debug, Deserialize)]
struct UploadGrantEnvelope {
    ok: bool,
    upload: UploadGrant,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadGrant {
    url: String,
    token: String,
    token_env: String,
    destination_path: String,
    expires_at: i64,
    // Intentionally decoded only so schema drift is caught. Never execute or
    // log this shell command because it projects the upload bearer externally.
    #[serde(rename = "command")]
    _command: String,
}

const VERIFY_HANDOFF_PYTHON: &str = r#"import hashlib, os, pathlib, stat, sys, uuid
staging_root = pathlib.Path(sys.argv[1]).resolve(strict=True)
workspace = pathlib.Path("/workspace/ashler-platform").resolve(strict=True)
if workspace not in staging_root.parents or staging_root.relative_to(workspace).as_posix() != ".scaffold/omp-handoff-staging":
    raise SystemExit(20)
rel = pathlib.PurePosixPath(sys.argv[2])
if rel.is_absolute() or not rel.parts or any(p in ("", ".", "..") for p in rel.parts):
    raise SystemExit(21)
staged = staging_root.joinpath(*rel.parts)
expected_sha, expected_count = sys.argv[3], int(sys.argv[4])

def exact_file(path, require_mode):
    try:
        fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    except (FileNotFoundError, OSError):
        return False
    with os.fdopen(fd, "rb") as f:
        st = os.fstat(f.fileno())
        if not stat.S_ISREG(st.st_mode) or stat.S_IMODE(st.st_mode) != require_mode:
            return False
        h, count = hashlib.sha256(), 0
        while True:
            block = f.read(1024 * 1024)
            if not block: break
            count += len(block); h.update(block)
    return count == expected_count and h.hexdigest() == expected_sha

def secure_dirs(root, parts):
    current = root
    for part in parts:
        candidate = current / part
        try:
            candidate.mkdir(mode=0o700)
        except FileExistsError:
            st = candidate.lstat()
            if not stat.S_ISDIR(st.st_mode) or stat.S_ISLNK(st.st_mode):
                raise SystemExit(25)
            os.chmod(candidate, 0o700, follow_symlinks=False)
        current = candidate
    return current

try:
    try:
        staged_resolved = staged.resolve(strict=True)
    except (FileNotFoundError, OSError):
        raise SystemExit(22)
    if staging_root not in staged_resolved.parents or staged_resolved != staged:
        raise SystemExit(22)
    if not exact_file(staged, 0o600):
        raise SystemExit(22)
    sessions = pathlib.Path("/workspace/.omp/profiles/scaffold-host/agent/sessions")
    parent = secure_dirs(pathlib.Path("/workspace"), sessions.relative_to("/workspace").parts + rel.parts[:-1])
    target = parent / rel.name
    try:
        if target.exists() or target.is_symlink():
            if target.is_symlink() or not exact_file(target, 0o600):
                raise SystemExit(26)
        else:
            temporary = parent / (".comet-handoff-" + uuid.uuid4().hex)
            fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o600)
            try:
                with os.fdopen(fd, "wb") as out, staged.open("rb") as source:
                    while True:
                        block = source.read(1024 * 1024)
                        if not block: break
                        out.write(block)
                    out.flush(); os.fsync(out.fileno())
                os.chmod(temporary, 0o600, follow_symlinks=False)
                try:
                    os.link(temporary, target, follow_symlinks=False)
                    temporary.unlink()
                except FileExistsError:
                    if target.is_symlink() or not exact_file(target, 0o600):
                        raise SystemExit(26)
                    temporary.unlink()
            finally:
                try: temporary.unlink()
                except FileNotFoundError: pass
        if not exact_file(target, 0o600):
            raise SystemExit(27)
    finally:
        pass
    print("verified")
finally:
    try: staged.unlink()
    except FileNotFoundError: pass
"#;

fn device_join_grant_id(credential: &str) -> Option<String> {
    let mut parts = credential.split('.');
    let prefix = parts.next()?;
    let grant_id = parts.next()?;
    let secret = parts.next()?;
    if prefix != "cg1"
        || grant_id.len() != 32
        || !grant_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || secret.is_empty()
        || parts.next().is_some()
    {
        return None;
    }
    Some(grant_id.to_string())
}

#[derive(Debug, Clone)]
pub struct DeviceJoinGrantRequest {
    pub principal_subject: String,
    pub scope: CollaborationScope,
    pub sandbox_id: String,
    pub device_id: String,
    pub capabilities: Vec<String>,
    pub expires_in_seconds: u32,
}

pub struct DeviceJoinCredential {
    credential: String,
    grant_id: String,
    join_expires_at: i64,
    control_expires_at: i64,
}

impl fmt::Debug for DeviceJoinCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceJoinCredential")
            .field("credential", &"<redacted>")
            .field("grant_id", &self.grant_id)
            .field("join_expires_at", &self.join_expires_at)
            .field("control_expires_at", &self.control_expires_at)
            .finish()
    }
}

#[async_trait]
pub trait DeviceJoinGrantProvider: Send + Sync {
    async fn mint(
        &self,
        request: &DeviceJoinGrantRequest,
        cancellation: &CancellationToken,
    ) -> Result<DeviceJoinCredential, ScaffoldError>;
}

#[derive(Clone)]
pub struct EdgeDeviceJoinGrantClient {
    http: reqwest::Client,
    origin: Url,
    bearer: Arc<dyn TokenSource>,
}

impl EdgeDeviceJoinGrantClient {
    pub fn new(
        origin: impl AsRef<str>,
        bearer: Arc<dyn TokenSource>,
    ) -> Result<Self, ScaffoldError> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(REQUEST_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            origin: exact_origin(origin.as_ref())?,
            bearer,
        })
    }
}

#[async_trait]
impl DeviceJoinGrantProvider for EdgeDeviceJoinGrantClient {
    async fn mint(
        &self,
        request: &DeviceJoinGrantRequest,
        cancellation: &CancellationToken,
    ) -> Result<DeviceJoinCredential, ScaffoldError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            grant: String,
            expires_at: i64,
            access_expires_at: Option<i64>,
        }

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            target_device_id: &'a str,
            session_id: &'a str,
            deployment_id: &'a str,
            sandbox_id: &'a str,
            capabilities: &'a [String],
            ttl_seconds: u32,
        }

        let session_id = request
            .scope
            .session_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or(ScaffoldError::DeviceJoinGrantUnavailable)?;
        let deployment_id = request
            .scope
            .deployment_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or(ScaffoldError::DeviceJoinGrantUnavailable)?;
        if request.sandbox_id.trim().is_empty() {
            return Err(ScaffoldError::DeviceJoinGrantUnavailable);
        }
        let body = Request {
            target_device_id: &request.device_id,
            session_id,
            deployment_id,
            sandbox_id: &request.sandbox_id,
            capabilities: &request.capabilities,
            ttl_seconds: request.expires_in_seconds,
        };

        let bearer = self
            .bearer
            .token()
            .await
            .filter(|token| !token.trim().is_empty())
            .ok_or(ScaffoldError::DeviceJoinGrantUnavailable)?;
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .expect("validated HTTP origin")
            .extend(JOIN_GRANT_PATH);
        let send = self
            .http
            .post(url)
            .bearer_auth(&bearer)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&body)
            .send();
        tokio::pin!(send);
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ScaffoldError::Cancelled),
            response = &mut send => response.map_err(|_| ScaffoldError::DeviceJoinGrantUnavailable)?,
        };
        drop(bearer);
        if !response.status().is_success() {
            return Err(ScaffoldError::DeviceJoinGrantUnavailable);
        }
        let response: Response = response
            .json()
            .await
            .map_err(|_| ScaffoldError::DeviceJoinGrantUnavailable)?;
        let now = now_ms();
        let control_expires_at = response.access_expires_at.unwrap_or_else(|| {
            response
                .expires_at
                .saturating_sub(i64::from(request.expires_in_seconds) * 1000)
                .saturating_add(DEVICE_ACCESS_TTL_MS)
        });
        let grant_id = device_join_grant_id(&response.grant)
            .filter(|_| response.expires_at > now)
            .filter(|_| control_expires_at > response.expires_at)
            .filter(|_| !request.principal_subject.trim().is_empty())
            .filter(|_| !request.sandbox_id.trim().is_empty())
            .ok_or(ScaffoldError::DeviceJoinGrantUnavailable)?;
        Ok(DeviceJoinCredential {
            credential: response.grant,
            grant_id,
            join_expires_at: response.expires_at,
            control_expires_at,
        })
    }
}

pub struct UnavailableDeviceJoinGrantProvider;

#[async_trait]
impl DeviceJoinGrantProvider for UnavailableDeviceJoinGrantProvider {
    async fn mint(
        &self,
        _request: &DeviceJoinGrantRequest,
        _cancellation: &CancellationToken,
    ) -> Result<DeviceJoinCredential, ScaffoldError> {
        Err(ScaffoldError::DeviceJoinGrantUnavailable)
    }
}

#[derive(Clone)]
pub struct ScaffoldRuntime {
    inner: Arc<ScaffoldRuntimeInner>,
}

struct ScaffoldRuntimeInner {
    client: ScaffoldClient,
    edge_origin: String,
    grants: Arc<dyn DeviceJoinGrantProvider>,
    environments: Mutex<BTreeMap<String, SessionEnvironment>>,
    watch_tx: watch::Sender<ScaffoldEnvironmentSnapshot>,
}

impl ScaffoldRuntime {
    pub fn new(
        client: ScaffoldClient,
        edge_origin: impl Into<String>,
        grants: Arc<dyn DeviceJoinGrantProvider>,
    ) -> Self {
        let snapshot = ScaffoldEnvironmentSnapshot {
            environments: Vec::new(),
            refreshed_at: now_ms(),
        };
        let (watch_tx, _) = watch::channel(snapshot);
        Self {
            inner: Arc::new(ScaffoldRuntimeInner {
                client,
                edge_origin: edge_origin.into(),
                grants,
                environments: Mutex::new(BTreeMap::new()),
                watch_tx,
            }),
        }
    }

    pub fn watch(&self) -> watch::Receiver<ScaffoldEnvironmentSnapshot> {
        self.inner.watch_tx.subscribe()
    }

    /// Explicit event-driven refresh. There is deliberately no polling task.
    pub async fn refresh(
        &self,
        scope: &CollaborationScope,
        cancellation: &CancellationToken,
    ) -> Result<ScaffoldEnvironmentSnapshot, ScaffoldError> {
        let environments = self.inner.client.list(scope, cancellation).await?;
        {
            let mut current = lock(&self.inner.environments);
            current.clear();
            for environment in environments {
                current.insert(
                    environment_sandbox_id(&environment)?.to_string(),
                    environment,
                );
            }
        }
        Ok(self.publish())
    }

    pub async fn control(
        &self,
        control: ScaffoldEnvironmentControl,
        cancellation: &CancellationToken,
    ) -> Result<ScaffoldEnvironmentControlResult, ScaffoldError> {
        let mut control_grant = None;
        let (environment, attached_device_id, run_id, handoff) = match control {
            ScaffoldEnvironmentControl::Inspect { sandbox_id, scope } => (
                self.inner
                    .client
                    .inspect(&sandbox_id, &scope, cancellation)
                    .await?,
                None,
                None,
                None,
            ),
            ScaffoldEnvironmentControl::Create {
                scope,
                name,
                source_ref,
                region,
                runtime_mode,
            } => (
                self.inner
                    .client
                    .create(
                        &scope,
                        name.as_deref(),
                        source_ref.as_deref(),
                        region.as_deref(),
                        runtime_mode,
                        cancellation,
                    )
                    .await?,
                None,
                None,
                None,
            ),
            ScaffoldEnvironmentControl::Pause { sandbox_id, scope } => (
                self.inner
                    .client
                    .pause(&sandbox_id, &scope, cancellation)
                    .await?,
                None,
                None,
                None,
            ),
            ScaffoldEnvironmentControl::Resume { sandbox_id, scope } => (
                self.inner
                    .client
                    .resume(&sandbox_id, &scope, cancellation)
                    .await?,
                None,
                None,
                None,
            ),
            ScaffoldEnvironmentControl::Stop { sandbox_id, scope } => (
                self.inner
                    .client
                    .stop(&sandbox_id, &scope, cancellation)
                    .await?,
                None,
                None,
                None,
            ),
            ScaffoldEnvironmentControl::Attach { sandbox_id, scope } => {
                let environment = self
                    .inner
                    .client
                    .inspect(&sandbox_id, &scope, cancellation)
                    .await?;
                let (device_id, run_id, grant) = self
                    .attach(&sandbox_id, &scope, &environment, cancellation)
                    .await?;
                control_grant = Some(grant);
                (environment, Some(device_id), Some(run_id), None)
            }
            ScaffoldEnvironmentControl::HandoffOmpSession {
                sandbox_id,
                scope,
                local_candidate_id,
            } => {
                let environment = self
                    .inner
                    .client
                    .inspect(&sandbox_id, &scope, cancellation)
                    .await?;
                let (lifecycle, _) = scaffold_source(&environment)?;
                if !matches!(
                    lifecycle,
                    comet_proto::ScaffoldLifecycle::Ready
                        | comet_proto::ScaffoldLifecycle::AgentRunning
                ) {
                    return Err(ScaffoldError::OmpSessionHandoffFailed);
                }
                let artifact = crate::local_sessions::capture_omp_artifact(&local_candidate_id)
                    .map_err(|_| ScaffoldError::OmpSessionHandoffFailed)?;
                self.inner
                    .client
                    .handoff_omp_session(&sandbox_id, &artifact, cancellation)
                    .await?;
                let handoff = Some((artifact.native_session_id, artifact.cwd));
                (environment, None, None, handoff)
            }
        };
        let sandbox_id = environment_sandbox_id(&environment)?.to_string();
        lock(&self.inner.environments).insert(sandbox_id, environment.clone());
        self.publish();
        let room_projection = (attached_device_id.is_some() || handoff.is_some())
            .then(|| {
                Some(SessionRoomProjection {
                    project_id: environment.scope.project_id.clone(),
                    deployment_id: environment.scope.deployment_id.clone()?,
                    session_id: environment.scope.session_id.clone()?,
                })
            })
            .flatten();
        let (handoff_native_session_id, handoff_cwd) = handoff
            .map(|(native_id, cwd)| (Some(native_id), Some(cwd)))
            .unwrap_or((None, None));
        Ok(ScaffoldEnvironmentControlResult {
            environment,
            attached_device_id,
            run_id,
            room_projection,
            control_grant,
            handoff_native_session_id,
            handoff_cwd,
        })
    }

    async fn attach(
        &self,
        sandbox_id: &str,
        scope: &CollaborationScope,
        environment: &SessionEnvironment,
        cancellation: &CancellationToken,
    ) -> Result<(String, String, ScaffoldControlGrant), ScaffoldError> {
        let (lifecycle, _) = scaffold_source(environment)?;
        if !matches!(
            lifecycle,
            comet_proto::ScaffoldLifecycle::Starting
                | comet_proto::ScaffoldLifecycle::Ready
                | comet_proto::ScaffoldLifecycle::AgentRunning
        ) {
            return Err(ScaffoldError::InvalidResponse(format!(
                "sandbox {sandbox_id} is not ready"
            )));
        }
        let deployment_id = scope
            .deployment_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ScaffoldError::InvalidScope("deploymentId is required to attach".into())
            })?;
        let session_id = scope
            .session_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| ScaffoldError::InvalidScope("sessionId is required to attach".into()))?;

        // Probe first so missing installations fail deterministically without minting a credential.
        let probe_argv = vec![
            "sh".to_string(),
            "-lc".to_string(),
            "command -v comet >/dev/null 2>&1".to_string(),
        ];
        let probe = self
            .inner
            .client
            .exec(
                sandbox_id,
                &ExecBody {
                    argv: &probe_argv,
                    mode: "inline",
                    timeout_ms: 10_000,
                },
                cancellation,
            )
            .await?;
        if !probe.ok || probe.exit_code != Some(0) {
            return Err(ScaffoldError::CometNotInstalled);
        }

        let device_id = format!("comet-scaffold-{sandbox_id}");
        let request = DeviceJoinGrantRequest {
            principal_subject: environment.owner_principal.clone(),
            scope: scope.clone(),
            sandbox_id: sandbox_id.to_string(),
            device_id: device_id.clone(),
            capabilities: vec![
                CAPABILITY_SESSION_READ.into(),
                CAPABILITY_SESSION_CHAT.into(),
                CAPABILITY_SESSION_CONTROL.into(),
                CAPABILITY_SESSION_ANNOTATE.into(),
                CAPABILITY_SESSION_FILES.into(),
                CAPABILITY_SESSION_ENVIRONMENT.into(),
            ],
            expires_in_seconds: JOIN_GRANT_TTL_SECONDS,
        };
        let join = self.inner.grants.mint(&request, cancellation).await?;
        let control_grant = ScaffoldControlGrant {
            id: join.grant_id.clone(),
            expires_at: join.control_expires_at,
            capabilities: request.capabilities.clone(),
        };

        // Deliver the one-time credential as a mode-0600 file, never as argv.
        // Comet consumes and removes this file before making the exchange.
        let bootstrap_path = format!(".comet-device-bootstrap-{}.json", uuid::Uuid::new_v4());
        let bootstrap = serde_json::json!({
            "deviceJoinGrant": join.credential,
            "projectId": scope.project_id,
            "deploymentId": deployment_id,
            "sessionId": session_id,
            "deviceId": device_id,
            "sandboxId": sandbox_id
        })
        .to_string();
        self.inner
            .client
            .put_file(sandbox_id, &bootstrap_path, &bootstrap, cancellation)
            .await?;
        let chmod_argv = vec![
            "chmod".to_string(),
            "600".to_string(),
            bootstrap_path.clone(),
        ];
        let chmod = match self
            .inner
            .client
            .exec(
                sandbox_id,
                &ExecBody {
                    argv: &chmod_argv,
                    mode: "inline",
                    timeout_ms: 10_000,
                },
                cancellation,
            )
            .await
        {
            Ok(response) if response.ok && response.exit_code == Some(0) => response,
            Ok(_) => {
                self.inner
                    .client
                    .remove_file(sandbox_id, &bootstrap_path, cancellation)
                    .await;
                return Err(ScaffoldError::InvalidResponse(
                    "could not secure the device bootstrap file".into(),
                ));
            }
            Err(error) => {
                self.inner
                    .client
                    .remove_file(sandbox_id, &bootstrap_path, cancellation)
                    .await;
                return Err(error);
            }
        };
        let _ = chmod;
        let argv = vec![
            "comet".to_string(),
            "headless".to_string(),
            "--device-bootstrap-file".to_string(),
            bootstrap_path.clone(),
            "--edge-url".to_string(),
            self.inner.edge_origin.clone(),
        ];
        let started = match self
            .inner
            .client
            .exec(
                sandbox_id,
                &ExecBody {
                    argv: &argv,
                    mode: "background",
                    timeout_ms: 0,
                },
                cancellation,
            )
            .await
        {
            Ok(started) => started,
            Err(error) => {
                self.inner
                    .client
                    .remove_file(sandbox_id, &bootstrap_path, cancellation)
                    .await;
                return Err(error);
            }
        };
        if !started.ok {
            self.inner
                .client
                .remove_file(sandbox_id, &bootstrap_path, cancellation)
                .await;
            if started.error.as_deref() == Some("exec_spawn_failed") {
                return Err(ScaffoldError::CometNotInstalled);
            }
            return Err(ScaffoldError::InvalidResponse(
                started
                    .error
                    .unwrap_or_else(|| "sandbox exec rejected".into()),
            ));
        }
        let run_id = started.run_id.ok_or_else(|| {
            ScaffoldError::InvalidResponse("sandbox exec returned no runId".into())
        })?;
        Ok((device_id, run_id, control_grant))
    }

    fn publish(&self) -> ScaffoldEnvironmentSnapshot {
        let snapshot = ScaffoldEnvironmentSnapshot {
            environments: lock(&self.inner.environments).values().cloned().collect(),
            refreshed_at: now_ms(),
        };
        self.inner.watch_tx.send_replace(snapshot.clone());
        snapshot
    }
}

fn scaffold_source(
    environment: &SessionEnvironment,
) -> Result<(comet_proto::ScaffoldLifecycle, &str), ScaffoldError> {
    match &environment.source {
        SessionEnvironmentSource::Scaffold {
            sandbox_id,
            lifecycle,
            ..
        } => Ok((*lifecycle, sandbox_id)),
        SessionEnvironmentSource::Local => Err(ScaffoldError::InvalidResponse(
            "local environment has no Scaffold sandbox".into(),
        )),
    }
}

fn environment_sandbox_id(environment: &SessionEnvironment) -> Result<&str, ScaffoldError> {
    scaffold_source(environment).map(|(_, sandbox_id)| sandbox_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_rpc::StaticToken;
    use sha2::Digest as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    #[test]
    fn scaffold_origin_requires_https_except_for_loopback() {
        let token: Arc<dyn TokenSource> = Arc::new(StaticToken("token".into()));
        assert!(
            ScaffoldClient::new(
                "https://scaffold.internal.ashler.com",
                "project-a",
                token.clone()
            )
            .is_ok()
        );
        assert!(ScaffoldClient::new("http://127.0.0.1:8787", "project-a", token.clone()).is_ok());
        assert!(
            ScaffoldClient::new(
                "http://scaffold.internal.ashler.com",
                "project-a",
                token.clone()
            )
            .is_err()
        );
        assert!(
            ScaffoldClient::new(
                "https://scaffold.internal.ashler.com/api",
                "project-a",
                token
            )
            .is_err()
        );
    }

    fn scope() -> CollaborationScope {
        CollaborationScope {
            project_id: "project-a".into(),
            deployment_id: Some("deployment-a".into()),
            session_id: Some("session-a".into()),
            unknown: Default::default(),
        }
    }

    fn sandbox(status: &str) -> String {
        format!(
            r#"{{"sandbox":{{"id":"sandbox-a","status":"{status}","kind":"remote_code","runtimeProfile":"remote_code","region":"us-central1","ownerEmail":"alice@example.com","createdAt":"2026-08-04T00:00:00Z","updatedAt":"2026-08-04T00:01:00Z","lastActivityAt":"2026-08-04T00:02:00Z","links":{{"terminal":"https://terminal.example"}}}}}}"#
        )
    }

    #[test]
    fn accepts_the_additive_comet_remote_profile_from_code_sandboxes() {
        let envelope: SandboxEnvelope = serde_json::from_str(&sandbox("ready").replace(
            r#""runtimeProfile":"remote_code""#,
            r#""runtimeProfile":"comet_remote""#,
        ))
        .unwrap();
        let environment = envelope.sandbox.into_environment(scope()).unwrap();
        assert!(matches!(
            environment.source,
            SessionEnvironmentSource::Scaffold { .. }
        ));
    }

    async fn mock_server(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for body in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let read = stream.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    if let Some(header_end) =
                        bytes.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let content_length = String::from_utf8_lossy(&bytes[..header_end])
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= header_end + 4 + content_length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (origin, task)
    }

    #[tokio::test]
    async fn control_plane_methods_decode_lifecycle_and_preserve_http_contract() {
        let listed_sandbox: Value = serde_json::from_str(&sandbox("paused")).unwrap();
        let listed = serde_json::json!({
            "ok": true,
            "sandboxes": [listed_sandbox["sandbox"].clone()],
        })
        .to_string();
        let responses = vec![
            listed,
            sandbox("ready"),
            sandbox("creating"),
            sandbox("paused"),
            sandbox("resuming"),
            sandbox("stopped"),
        ];
        let (origin, captured) = mock_server(responses).await;
        let client = ScaffoldClient::new(
            &origin,
            "project-a",
            Arc::new(StaticToken("sc_rc_control_secret".into())),
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        assert_eq!(client.list(&scope(), &cancellation).await.unwrap().len(), 1);
        client
            .inspect("sandbox-a", &scope(), &cancellation)
            .await
            .unwrap();
        client
            .create(
                &scope(),
                Some("Comet"),
                Some("main"),
                Some("us-central1"),
                Some(ScaffoldRuntimeMode::Tilt),
                &cancellation,
            )
            .await
            .unwrap();
        client
            .pause("sandbox-a", &scope(), &cancellation)
            .await
            .unwrap();
        client
            .resume("sandbox-a", &scope(), &cancellation)
            .await
            .unwrap();
        let stopped = client
            .stop("sandbox-a", &scope(), &cancellation)
            .await
            .unwrap();
        assert!(matches!(
            stopped.source,
            SessionEnvironmentSource::Scaffold {
                lifecycle: comet_proto::ScaffoldLifecycle::Stopped,
                ..
            }
        ));

        let requests = captured.await.unwrap();
        let request_lines: Vec<_> = requests
            .iter()
            .map(|request| request.lines().next().unwrap())
            .collect();
        assert_eq!(
            request_lines,
            [
                "GET /api/code-sandboxes HTTP/1.1",
                "GET /api/code-sandboxes/sandbox-a HTTP/1.1",
                "POST /api/code-sandboxes HTTP/1.1",
                "POST /api/code-sandboxes/sandbox-a/pause HTTP/1.1",
                "POST /api/code-sandboxes/sandbox-a/resume HTTP/1.1",
                "POST /api/code-sandboxes/sandbox-a/stop HTTP/1.1",
            ]
        );
        assert!(requests.iter().all(|request| {
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer sc_rc_control_secret")
        }));
        assert!(requests[2].contains(r#""runtimeMode":"tilt""#));
        assert!(requests[2].contains(r#""version":"scaffold.comet-runtime.v1""#));
        assert!(requests[2].contains(r#""projectId":"project-a""#));
        assert!(requests[2].contains(r#""deploymentId":"deployment-a""#));
        assert!(requests[2].contains(r#""sessionId":"session-a""#));
    }

    #[tokio::test]
    async fn attach_keeps_credentials_out_of_process_arguments() {
        let join_expires_at = now_ms() + 60_000;
        let access_expires_at =
            join_expires_at - i64::from(JOIN_GRANT_TTL_SECONDS) * 1000 + DEVICE_ACCESS_TTL_MS;
        let responses = vec![
            sandbox("ready"),
            r#"{"ok":true,"exitCode":0}"#.into(),
            format!(
                r#"{{"grant":"cg1.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.narrow_secret","expiresAt":{join_expires_at}}}"#
            ),
            r#"{"ok":true}"#.into(),
            r#"{"ok":true,"exitCode":0}"#.into(),
            r#"{"ok":true,"runId":"run-a"}"#.into(),
        ];
        let (origin, captured) = mock_server(responses).await;
        let bearer: Arc<dyn TokenSource> = Arc::new(StaticToken("sc_rc_broad_secret".into()));
        let runtime = ScaffoldRuntime::new(
            ScaffoldClient::new(&origin, "project-a", bearer.clone()).unwrap(),
            "https://comet-edge.example",
            Arc::new(EdgeDeviceJoinGrantClient::new(&origin, bearer).unwrap()),
        );
        let result = runtime
            .control(
                ScaffoldEnvironmentControl::Attach {
                    sandbox_id: "sandbox-a".into(),
                    scope: scope(),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.attached_device_id.as_deref(),
            Some("comet-scaffold-sandbox-a")
        );
        assert_eq!(result.run_id.as_deref(), Some("run-a"));
        assert_eq!(
            result.room_projection,
            Some(SessionRoomProjection {
                project_id: "project-a".into(),
                deployment_id: "deployment-a".into(),
                session_id: "session-a".into(),
            })
        );
        assert_eq!(
            result.control_grant,
            Some(ScaffoldControlGrant {
                id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                expires_at: access_expires_at,
                capabilities: vec![
                    CAPABILITY_SESSION_READ.into(),
                    CAPABILITY_SESSION_CHAT.into(),
                    CAPABILITY_SESSION_CONTROL.into(),
                    CAPABILITY_SESSION_ANNOTATE.into(),
                    CAPABILITY_SESSION_FILES.into(),
                    CAPABILITY_SESSION_ENVIRONMENT.into(),
                ],
            })
        );
        let serialized_result = serde_json::to_string(&result).unwrap();
        assert!(serialized_result.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!serialized_result.contains("narrow_secret"));

        let requests = captured.await.unwrap();
        let exec_bodies: Vec<_> = requests
            .iter()
            .filter(|request| request.starts_with("POST /api/code-sandboxes/sandbox-a/exec "))
            .map(|request| request.split_once("\r\n\r\n").unwrap().1)
            .collect();
        assert_eq!(exec_bodies.len(), 3);
        assert!(
            exec_bodies
                .iter()
                .all(|body| !body.contains("sc_rc_broad_secret") && !body.contains("cg1."))
        );
        assert!(exec_bodies[1].contains(r#""chmod","600""#));
        assert!(exec_bodies[2].contains(r#""--device-bootstrap-file""#));
        assert!(!exec_bodies[2].contains("--device-join-grant"));
        assert!(exec_bodies[2].contains(r#""mode":"background""#));
        assert!(exec_bodies[2].contains(r#""timeoutMs":0"#));
        let bootstrap_request = requests
            .iter()
            .find(|request| request.starts_with("PUT /api/code-sandboxes/sandbox-a/files?"))
            .expect("bootstrap file upload");
        let bootstrap_body = bootstrap_request.split_once("\r\n\r\n").unwrap().1;
        let bootstrap_envelope: Value = serde_json::from_str(bootstrap_body).unwrap();
        let bootstrap: Value =
            serde_json::from_str(bootstrap_envelope["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            bootstrap["deviceJoinGrant"],
            "cg1.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.narrow_secret"
        );
        assert_eq!(bootstrap["deploymentId"], "deployment-a");
        let grant_body = requests[2].split_once("\r\n\r\n").unwrap().1;
        assert!(grant_body.contains(r#""sandboxId":"sandbox-a""#));
        assert!(grant_body.contains(r#""deploymentId":"deployment-a""#));
    }
    #[tokio::test]
    async fn omp_handoff_uses_one_use_raw_tar_and_verifies_without_starting_acp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let upload_url = format!("{origin}/one-use-upload");
        let responses = [
            serde_json::json!({
                "ok": true,
                "upload": {
                    "url": upload_url,
                    "token": "single_use_upload_secret",
                    "tokenEnv": "SCAFFOLD_UPLOAD_TOKEN",
                    "destinationPath": "/workspace/ashler-platform/.scaffold/omp-handoff-staging",
                    "expiresAt": now_ms() + 60_000,
                    "command": "SCAFFOLD_UPLOAD_TOKEN=single_use_upload_secret curl ..."
                }
            })
            .to_string(),
            "{}".to_string(),
            r#"{"ok":true,"exitCode":0,"stdout":"verified\n"}"#.to_string(),
        ];
        let captured = tokio::spawn(async move {
            let mut requests = Vec::new();
            for body in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let read = stream.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    if let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let len = String::from_utf8_lossy(&bytes[..header_end])
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= header_end + 4 + len {
                            break;
                        }
                    }
                }
                requests.push(bytes);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });

        let bytes = b"{\"type\":\"session\",\"id\":\"native-1\",\"cwd\":\"/repo\"}\n".to_vec();
        let artifact = OmpSessionArtifact {
            native_session_id: "native-1".into(),
            cwd: "/repo".into(),
            storage_relative_path: "by-cwd/native-1.jsonl".into(),
            sha256: format!("{:x}", sha2::Sha256::digest(&bytes)),
            byte_count: bytes.len() as u64,
            bytes: bytes.clone(),
        };
        let client = ScaffoldClient::new(
            &origin,
            "project-a",
            Arc::new(StaticToken("scaffold_control_secret".into())),
        )
        .unwrap();
        client
            .handoff_omp_session("sandbox-a", &artifact, &CancellationToken::new())
            .await
            .unwrap();

        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 3);
        let grant = String::from_utf8_lossy(&requests[0]);
        assert!(grant.starts_with("POST /api/code-sandboxes/sandbox-a/uploads HTTP/1.1"));
        assert!(grant.contains(r#"{"destinationPath":".scaffold/omp-handoff-staging"}"#));
        assert!(!grant.contains("single_use_upload_secret"));

        let upload_header_end = requests[1]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap();
        let upload_headers = String::from_utf8_lossy(&requests[1][..upload_header_end]);
        assert!(upload_headers.starts_with("POST /one-use-upload HTTP/1.1"));
        assert!(
            upload_headers
                .to_ascii_lowercase()
                .contains("content-type: application/x-tar")
        );
        assert!(upload_headers.contains("authorization: Bearer single_use_upload_secret"));
        let tar = &requests[1][upload_header_end + 4..];
        assert_eq!(
            &tar[.."by-cwd/native-1.jsonl".len()],
            b"by-cwd/native-1.jsonl"
        );
        assert_eq!(&tar[100..107], b"0000600");
        assert_eq!(&tar[512..512 + bytes.len()], bytes);

        let verify = String::from_utf8_lossy(&requests[2]);
        assert!(verify.starts_with("POST /api/code-sandboxes/sandbox-a/exec HTTP/1.1"));
        let verify_body = verify.split_once("\r\n\r\n").unwrap().1;
        assert!(verify_body.contains("/workspace/.omp/profiles/scaffold-host/agent/sessions"));
        assert!(verify_body.contains("/workspace/ashler-platform/.scaffold/omp-handoff-staging"));
        assert!(verify_body.contains("staged.unlink()"));
        assert!(verify_body.contains("by-cwd/native-1.jsonl"));
        assert!(verify_body.contains(&artifact.sha256));
        assert!(!verify_body.contains("single_use_upload_secret"));
        assert!(!verify_body.contains("omp acp"));
        assert!(!verify_body.contains("session/load"));
    }

    #[test]
    fn handoff_rejects_escaping_archive_paths_and_inexact_bytes() {
        let bytes = b"hello".to_vec();
        let mut artifact = OmpSessionArtifact {
            native_session_id: "native-1".into(),
            cwd: "/repo".into(),
            storage_relative_path: "../escape.jsonl".into(),
            sha256: format!("{:x}", sha2::Sha256::digest(&bytes)),
            byte_count: bytes.len() as u64,
            bytes,
        };
        assert!(validate_handoff_artifact(&artifact).is_err());
        artifact.storage_relative_path = "safe/native-1.jsonl".into();
        artifact.byte_count += 1;
        assert!(validate_handoff_artifact(&artifact).is_err());
    }
    #[test]
    fn materializer_process_is_exact_idempotent_and_rejects_mismatch() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::process::Command;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let workspace = root.join("ashler-platform");
        let staging = workspace.join(".scaffold/omp-handoff-staging/by-cwd");
        std::fs::create_dir_all(&staging).unwrap();
        let staged = staging.join("native-1.jsonl");
        let bytes = b"native omp bytes\n";
        let sha = format!("{:x}", sha2::Sha256::digest(bytes));
        let script = VERIFY_HANDOFF_PYTHON.replace("/workspace", root.to_str().unwrap());
        let run = |script: &str| {
            Command::new("python3")
                .args([
                    "-c",
                    script,
                    workspace
                        .join(".scaffold/omp-handoff-staging")
                        .to_str()
                        .unwrap(),
                    "by-cwd/native-1.jsonl",
                    &sha,
                    &bytes.len().to_string(),
                ])
                .output()
                .unwrap()
        };
        std::fs::write(&staged, bytes).unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600)).unwrap();
        let first = run(&script);
        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert!(!staged.exists(), "staged file must be consumed");
        let target = root.join(".omp/profiles/scaffold-host/agent/sessions/by-cwd/native-1.jsonl");
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(target.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        // Re-uploading identical bytes is an exact, safe no-op at the destination.
        std::fs::write(&staged, bytes).unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(run(&script).status.success());
        assert_eq!(std::fs::read(&target).unwrap(), bytes);

        // An existing mismatched destination fails closed and still cleans staging.
        std::fs::write(&target, b"different").unwrap();
        std::fs::write(&staged, bytes).unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mismatch = run(&script);
        assert!(!mismatch.status.success());
        assert!(!staged.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"different");
    }
}
