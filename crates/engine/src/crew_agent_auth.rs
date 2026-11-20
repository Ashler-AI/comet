//! Crew: the Agent Auth client (docs/crew/zgx-thin-port.md, slice 5), ported
//! from the Agent Auth part of Crew's `ScaffoldClient` (`crates/engine/src/
//! scaffold.rs`, Ashler-AI/comet). The sandbox control plane is slice 6's.
//!
//! Platform's endpoints, on the Scaffold authority's origin (`scaffoldUrl`):
//!
//! - `POST /api/agent-auth/v2/authority`: a short-lived inference authority
//!   for the signed-in principal, against the engine's Scaffold bearer;
//! - `POST /api/agent-auth/v2/{messages,responses}`: inference with that
//!   authority, for one conversation and, when the chat chose one, one account;
//! - `GET /api/agent-accounts`: the principal's shared accounts (the catalog's
//!   account choices, and Settings' list: `crew_accounts.rs`);
//! - `POST /api/agent-accounts/import`: a login signed in on this device
//!   handed to the shared pool (`crew_shared_login.rs`);
//! - `DELETE /api/agent-accounts/{id}`: a shared account revoked.
//!
//! The Scaffold bearer comes from the engine's verified profile
//! ([`crate::auth::ProfileTokens`]) on every request and is only ever an
//! `Authorization` header to this origin: never logged, never in an error,
//! never followed through a redirect. Response bodies are untrusted: errors
//! keep their status and code only.

use std::sync::Arc;
use std::time::Duration;

use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeron_harness::CancellationToken;
use zeron_rpc::{TokenError, TokenSource};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Accounts and authority answers are small; anything bigger isn't one.
const MAX_JSON_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiError {
    pub status: u16,
    pub code: String,
    pub retry_after: Option<Duration>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentAuthError {
    /// Not signed in to Crew (or signed in as someone else since).
    #[error("agent_auth_signed_out")]
    SignedOut,
    #[error("agent_auth_credential_unavailable")]
    CredentialUnavailable,
    #[error("agent_auth_request_cancelled")]
    Cancelled,
    #[error("agent_auth_request_failed")]
    Transport(#[source] reqwest::Error),
    #[error("agent_auth_response_invalid: {0}")]
    InvalidResponse(String),
    #[error("agent_auth_api_error:{}:{}", .0.status, .0.code)]
    Api(ApiError),
}

impl From<reqwest::Error> for AgentAuthError {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(error.without_url())
    }
}

impl AgentAuthError {
    pub(crate) fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api(error) => error.retry_after,
            _ => None,
        }
    }

    /// Worth trying again later: the network, a credential being verified,
    /// or a temporary refusal.
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::Transport(_) | Self::CredentialUnavailable => true,
            Self::Api(error) => matches!(error.status, 429 | 500 | 502 | 503 | 504),
            _ => false,
        }
    }

    /// Why, in a few words a person can read, to end a sentence ("…couldn't
    /// be loaded: {reason}."). A refusal keeps its code only (never a body,
    /// a request id or a token), redacted all the same.
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::SignedOut => "you're signed out of Crew".into(),
            Self::CredentialUnavailable => "your Crew sign-in is still being checked".into(),
            Self::Cancelled => "it was cancelled".into(),
            Self::Transport(_) => "Agent Auth can't be reached".into(),
            Self::InvalidResponse(_) => "Agent Auth's answer wasn't understood".into(),
            Self::Api(error) => {
                let code = zeron_harness::redact::redact_output(&error.code.replace('_', " "));
                match error.status {
                    404 => format!("Agent Auth doesn't know it ({code})"),
                    429 | 500 | 502 | 503 | 504 => format!("Agent Auth is unavailable right now ({code})"),
                    _ => format!("Agent Auth refused it ({code})"),
                }
            }
        }
    }

    /// Agent Auth has no such account (any more).
    pub(crate) fn not_found(&self) -> bool {
        matches!(self, Self::Api(error) if error.status == 404)
    }
}

/// The Scaffold bearer as sent, and a fingerprint of it: cached authority is
/// for one credential, and a new one (another sign-in) discards it.
pub(crate) struct Credential {
    pub value: String,
    pub generation: u64,
}

/// A v2 inference authority (`POST /api/agent-auth/v2/authority`).
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentInferenceAuthority {
    pub contract_version: u8,
    pub token: String,
    pub token_type: String,
    pub authority_id: String,
    pub principal_id: String,
    pub authority_scope: Option<String>,
    pub expires_at: String,
}

/// Never prints the authority token.
impl std::fmt::Debug for AgentInferenceAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentInferenceAuthority")
            .field("contract_version", &self.contract_version)
            .field("authority_id", &self.authority_id)
            .field("principal_id", &self.principal_id)
            .field("authority_scope", &self.authority_scope)
            .field("expires_at", &self.expires_at)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// One shared account (`GET /api/agent-accounts`, and an import's answer).
/// Only what the catalog and Settings show; the credentials stay with Agent
/// Auth.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteAgentAccount {
    pub id: String,
    pub provider: String,
    #[serde(default)]
    pub provider_account_id: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub organization: Option<String>,
    /// Platform's name for the plan; Crew's engine re-sends it as `planLabel`
    /// (its Settings' `SharedAccount`), so either is read.
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    plan_label: Option<String>,
    /// `connected`, `disabled`, `attention_required`, `revoked`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub usage_windows: Vec<RemoteUsageWindow>,
}

/// A plan window as Agent Auth reads it (`{modelPattern, label, usedFraction,
/// resetAt}`), every field optional: one odd window never loses the list.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteUsageWindow {
    /// The models it limits: a family's (`claude-*`, `gpt-*`), or one
    /// model's or tier's (`claude-opus-*`, `gpt-5.3-codex-spark`).
    #[serde(default)]
    pub model_pattern: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub used_fraction: Option<f64>,
    #[serde(default)]
    pub reset_at: Option<String>,
    #[serde(default)]
    pub resets_at: Option<String>,
}

impl RemoteAgentAccount {
    /// Crew's Settings' test: an account Agent Auth can route to.
    pub(crate) fn healthy(&self) -> bool {
        matches!(self.status.as_str(), "connected" | "active" | "ready")
    }

    pub(crate) fn plan_label(&self) -> Option<&str> {
        [self.plan_label.as_deref(), self.plan.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|plan| !plan.is_empty())
    }
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct RemoteAgentAccounts {
    accounts: Vec<RemoteAgentAccount>,
}

/// A login signed in on this device, for the shared pool
/// (`POST /api/agent-accounts/import`), as Crew's `AgentAccountCredentialImport`.
/// Never logged; kept in memory only until Agent Auth acknowledges it.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentAccountImport {
    /// `anthropic` or `openai`.
    pub provider: String,
    /// Who the login is at the provider, as a slot is keyed: Claude's account
    /// uuid, ChatGPT's `user::workspace`.
    pub provider_account_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub capabilities: Vec<String>,
    pub credential: AgentAccountCredential,
}

/// The OAuth grant itself.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentAccountCredential {
    pub access_token: String,
    pub refresh_token: String,
    /// RFC 3339.
    pub expires_at: String,
    pub scopes: Vec<String>,
}

/// Never prints the grant.
impl std::fmt::Debug for AgentAccountImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentAccountImport")
            .field("provider", &self.provider)
            .field("provider_account_id", &self.provider_account_id)
            .field("email", &self.email)
            .field("credential", &"<redacted>")
            .finish()
    }
}

/// An inference request the relay forwards.
pub(crate) struct AgentInferenceProxyRequest<'a> {
    /// `messages` or `responses`.
    pub endpoint: &'a str,
    pub query: Option<&'a str>,
    pub authority: &'a AgentInferenceAuthority,
    pub conversation_id: &'a str,
    pub requested_account_id: Option<&'a str>,
    pub request_id: &'a str,
    pub headers: reqwest::header::HeaderMap,
    pub content_length: u64,
    pub body: reqwest::Body,
    pub cancellation: &'a CancellationToken,
}

/// Headers a child may never set on the authority's behalf.
pub(crate) const AGENT_AUTH_HEADERS: &[&str] = &[
    "x-api-key",
    "x-goog-api-key",
    "x-agent-auth-owner-subject",
    "x-agent-auth-session-id",
    "x-agent-auth-provider",
    "x-agent-auth-model",
    "x-agent-auth-harness",
    "x-agent-auth-source",
    "x-agent-auth-environment",
    "x-agent-auth-lifecycle-epoch",
    "x-agent-auth-request-id",
    "x-agent-auth-authority-scope",
    "x-agent-auth-routing-mode",
    "x-agent-auth-requested-account-id",
    "x-agent-auth-account-id",
    "x-agent-auth-conversation-id",
    "x-agent-auth-internal-secret",
];

#[derive(Clone)]
pub(crate) struct AgentAuthClient {
    http: reqwest::Client,
    /// Inference streams have no total deadline.
    inference_http: reqwest::Client,
    origin: Url,
    bearer: Arc<dyn TokenSource>,
}

impl std::fmt::Debug for AgentAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentAuthClient")
            .field("origin", &self.origin.as_str())
            .field("bearer", &"<provider>")
            .finish()
    }
}

fn http_client(total_timeout: Option<Duration>) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(REQUEST_TIMEOUT)
        // An authorization-bearing request never follows a redirect elsewhere.
        .redirect(reqwest::redirect::Policy::none());
    if let Some(timeout) = total_timeout {
        builder = builder.timeout(timeout);
    }
    builder.build()
}

/// An exact HTTPS origin (HTTP only on loopback), as Crew's client takes it.
fn exact_origin(value: &str) -> Result<Url, AgentAuthError> {
    let invalid = || AgentAuthError::InvalidResponse("the Scaffold URL must be an exact HTTPS origin (HTTP only on loopback)".into());
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

fn response_retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some((date.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().unwrap_or_default())
}

/// The status and code of a refusal; free-form messages stay out.
fn api_error(status: StatusCode, body: &[u8], retry_after: Option<Duration>) -> AgentAuthError {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        error: serde_json::Value,
    }
    let error = serde_json::from_slice::<Envelope>(body).ok().map(|envelope| envelope.error);
    let code = error
        .as_ref()
        .and_then(|error| {
            error
                .as_str()
                .or_else(|| error.get("error").and_then(serde_json::Value::as_str))
                .or_else(|| error.get("code").and_then(serde_json::Value::as_str))
        })
        .filter(|code| code.len() <= 128 && code.bytes().all(|b| b.is_ascii_graphic()))
        .unwrap_or("agent_auth_request_rejected")
        .to_owned();
    AgentAuthError::Api(ApiError {
        status: status.as_u16(),
        code,
        retry_after,
    })
}

impl AgentAuthClient {
    /// The client for the Scaffold authority at `origin`, sending `bearer`'s
    /// credential.
    pub(crate) fn new(origin: &str, bearer: Arc<dyn TokenSource>) -> Result<Self, AgentAuthError> {
        Ok(Self {
            http: http_client(Some(REQUEST_TIMEOUT))?,
            inference_http: http_client(None)?,
            origin: exact_origin(origin)?,
            bearer,
        })
    }

    /// The Scaffold credential now (and its fingerprint).
    pub(crate) async fn credential(&self) -> Result<Credential, AgentAuthError> {
        match self.bearer.token().await {
            Ok(value) if !value.trim().is_empty() => {
                let digest = Sha256::digest(value.as_bytes());
                let mut generation = [0_u8; 8];
                generation.copy_from_slice(&digest[..8]);
                Ok(Credential {
                    value,
                    generation: u64::from_be_bytes(generation),
                })
            }
            Ok(_) | Err(TokenError::SignedOut) => Err(AgentAuthError::SignedOut),
            Err(TokenError::TemporarilyUnavailable(_)) => Err(AgentAuthError::CredentialUnavailable),
        }
    }

    async fn json<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
        cancellation: &CancellationToken,
    ) -> Result<T, AgentAuthError> {
        let url = self.origin.join(path).expect("static Agent Auth path");
        let bytes = self.call(method, url, body, cancellation).await?;
        serde_json::from_slice(&bytes).map_err(|_| AgentAuthError::InvalidResponse("not the expected JSON".into()))
    }

    /// One authorized request; a success's body (at most [`MAX_JSON_BYTES`]).
    async fn call(
        &self,
        method: Method,
        url: Url,
        body: Option<&serde_json::Value>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, AgentAuthError> {
        let credential = self.credential().await?;
        let mut request = self
            .http
            .request(method, url)
            .header(reqwest::header::ACCEPT, "application/json")
            .bearer_auth(&credential.value);
        drop(credential);
        if let Some(body) = body {
            request = request.json(body);
        }
        let mut response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AgentAuthError::Cancelled),
            response = request.send() => response?,
        };
        let status = response.status();
        let retry_after = response_retry_after(&response);
        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(AgentAuthError::Cancelled),
                chunk = response.chunk() => chunk?,
            };
            let Some(chunk) = chunk else { break };
            if bytes.len() + chunk.len() > MAX_JSON_BYTES {
                return Err(AgentAuthError::InvalidResponse("the answer is too large".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(api_error(status, &bytes, retry_after));
        }
        Ok(bytes)
    }

    /// A short-lived inference authority for the signed-in principal.
    pub(crate) async fn issue_authority(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<AgentInferenceAuthority, AgentAuthError> {
        self.json(
            Method::POST,
            "/api/agent-auth/v2/authority",
            Some(&serde_json::json!({})),
            cancellation,
        )
        .await
    }

    /// The principal's shared accounts.
    pub(crate) async fn list_accounts(&self) -> Result<Vec<RemoteAgentAccount>, AgentAuthError> {
        let listed: RemoteAgentAccounts = self
            .json(Method::GET, "/api/agent-accounts", None, &CancellationToken::new())
            .await?;
        Ok(listed.accounts)
    }

    /// Hand a login to the shared pool; the account Agent Auth keeps it as.
    pub(crate) async fn import_account(
        &self,
        import: &AgentAccountImport,
    ) -> Result<RemoteAgentAccount, AgentAuthError> {
        #[derive(Deserialize)]
        struct Imported {
            account: RemoteAgentAccount,
        }
        let body = serde_json::to_value(import)
            .map_err(|_| AgentAuthError::InvalidResponse("the account couldn't be encoded".into()))?;
        let imported: Imported = self
            .json(
                Method::POST,
                "/api/agent-accounts/import",
                Some(&body),
                &CancellationToken::new(),
            )
            .await?;
        Ok(imported.account)
    }

    /// Revoke one shared account. (One Agent Auth doesn't know answers 404:
    /// `agent_account_not_found`.)
    pub(crate) async fn revoke_account(&self, account_id: &str) -> Result<(), AgentAuthError> {
        let id = account_id.trim();
        if id.is_empty()
            || id.len() > 256
            || matches!(id, "." | "..")
            || id.chars().any(|c| c == '/' || c == '\\' || c.is_whitespace() || c.is_control())
        {
            return Err(AgentAuthError::InvalidResponse("not a shared account's id".into()));
        }
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .map_err(|_| AgentAuthError::InvalidResponse("the Scaffold URL has no path".into()))?
            .clear()
            .extend(["api", "agent-accounts", id]);
        self.call(Method::DELETE, url, None, &CancellationToken::new())
            .await
            .map(drop)
    }

    /// Forward one inference request with `authority`, streaming both ways.
    pub(crate) async fn proxy(
        &self,
        proxy: AgentInferenceProxyRequest<'_>,
    ) -> Result<reqwest::Response, AgentAuthError> {
        let AgentInferenceProxyRequest {
            endpoint,
            query,
            authority,
            conversation_id,
            requested_account_id,
            request_id,
            mut headers,
            content_length,
            body,
            cancellation,
        } = proxy;
        let path = match endpoint {
            "responses" => "/api/agent-auth/v2/responses",
            "messages" => "/api/agent-auth/v2/messages",
            _ => return Err(AgentAuthError::InvalidResponse("unsupported inference endpoint".into())),
        };
        for name in [
            reqwest::header::AUTHORIZATION,
            reqwest::header::HOST,
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::CONNECTION,
        ] {
            headers.remove(name);
        }
        for name in AGENT_AUTH_HEADERS {
            headers.remove(*name);
        }
        let mut url = self.origin.join(path).expect("static inference path");
        url.set_query(query);
        let mut request = self
            .inference_http
            .post(url)
            .headers(headers)
            .bearer_auth(&authority.token)
            .header(reqwest::header::CONTENT_LENGTH, content_length)
            .header("x-agent-auth-request-id", request_id)
            .header("x-agent-auth-conversation-id", conversation_id);
        if let Some(scope) = authority.authority_scope.as_deref() {
            request = request.header("x-agent-auth-authority-scope", scope);
        }
        if let Some(account) = requested_account_id {
            request = request.header("x-agent-auth-account-id", account);
        }
        let request = request.body(body);
        tokio::select! {
            response = request.send() => response.map_err(AgentAuthError::from),
            () = cancellation.cancelled() => Err(AgentAuthError::Cancelled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_exact_https_or_loopback_origin_is_an_authority() {
        for good in ["https://scaffold.example", "https://scaffold.example/", "http://127.0.0.1:8787"] {
            assert!(exact_origin(good).is_ok(), "{good}");
        }
        for bad in [
            "http://scaffold.example",
            "https://scaffold.example/api",
            "https://user@scaffold.example",
            "https://scaffold.example/?x=1",
            "file:///tmp",
        ] {
            assert!(exact_origin(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn refusals_keep_their_status_and_code_only() {
        let error = api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            br#"{"error":{"code":"temporarily_unavailable","message":"secret detail"}}"#,
            Some(Duration::from_secs(2)),
        );
        assert!(error.is_transient());
        assert_eq!(error.retry_after(), Some(Duration::from_secs(2)));
        let shown = error.to_string();
        assert_eq!(shown, "agent_auth_api_error:503:temporarily_unavailable");
        assert!(!shown.contains("secret detail"));
        let opaque = api_error(StatusCode::FORBIDDEN, b"<html>nope</html>", None);
        assert_eq!(opaque.to_string(), "agent_auth_api_error:403:agent_auth_request_rejected");
        assert!(!opaque.is_transient());
    }

    #[test]
    fn an_authority_never_prints_its_token() {
        let authority = AgentInferenceAuthority {
            contract_version: 2,
            token: "remote-secret".into(),
            token_type: "Bearer".into(),
            authority_id: "authority-1".into(),
            principal_id: "identity:owner-1".into(),
            authority_scope: None,
            expires_at: "2099-01-01T00:00:00Z".into(),
        };
        assert!(!format!("{authority:?}").contains("remote-secret"));
    }
}
