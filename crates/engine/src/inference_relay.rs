use bytes::Bytes;
use chrono::{DateTime, TimeDelta, Utc};
use futures::{Stream, StreamExt};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{AUTHORIZATION, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderMap};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener as StdTcpListener};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, Semaphore, broadcast};

use comet_harness::InferenceRoute;
use comet_proto::HarnessId;

use crate::scaffold::{AgentInferenceAuthority, AgentInferenceProxyRequest, ScaffoldClient};
use crate::{EngineError, new_id};

#[derive(Clone)]
struct InferenceRouteRequest {
    logical_session_id: String,
    provider: String,
    model: String,
    requested_account_id: Option<String>,
    lifecycle_epoch: u64,
}

const MAX_CONNECTIONS: usize = 64;
const REFRESH_SKEW_SECONDS: i64 = 60;
const RETIRED_ROUTE_TTL: Duration = Duration::from_secs(30 * 60);

type BoxError = Box<dyn Error + Send + Sync>;
type RelayBody = UnsyncBoxBody<Bytes, BoxError>;
type RelayByteStream =
    Pin<Box<dyn futures::Stream<Item = Result<Bytes, BoxError>> + Send + 'static>>;

#[derive(Clone)]
pub(crate) struct InferenceRelay {
    inner: Arc<Inner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExpiredRoute {
    pub(crate) logical_session_id: String,
    pub(crate) lifecycle_epoch: u64,
}

struct Inner {
    client: ScaffoldClient,
    port: u16,
    route_state: Mutex<RouteState>,
    route_expired_tx: broadcast::Sender<ExpiredRoute>,
    next_lifecycle_epoch: AtomicU64,
}

struct Route {
    request: InferenceRouteRequest,
    /// Original local chat id when Agent Auth needs a UUID projection.
    local_session_id: Option<String>,
    owner_subject: String,
    authority: AsyncMutex<AuthorityState>,
    cancellation: comet_harness::CancellationToken,
}

impl Route {
    fn session_id(&self) -> &str {
        self.local_session_id
            .as_deref()
            .unwrap_or(&self.request.logical_session_id)
    }
}

struct AuthorityState {
    authority: AgentInferenceAuthority,
    expires_at: DateTime<Utc>,
}

#[derive(Default)]
struct RouteState {
    active: HashMap<String, Arc<Route>>,
    retired: HashMap<String, RetiredRoute>,
    /// Last account confirmed by Agent Auth response headers for each local chat.
    selected_accounts: HashMap<String, String>,
}

struct RetiredRoute {
    logical_session_id: String,
    local_session_id: Option<String>,
    owner_subject: String,
    provider: String,
    lifecycle_epoch: u64,
    retired_at: Instant,
}

impl RetiredRoute {
    fn session_id(&self) -> &str {
        self.local_session_id
            .as_deref()
            .unwrap_or(&self.logical_session_id)
    }
}

impl RouteState {
    fn prune_retired(&mut self) {
        self.retired
            .retain(|_, route| route.retired_at.elapsed() < RETIRED_ROUTE_TTL);
    }

    fn take_retired_token(
        &mut self,
        session_id: &str,
        owner_subject: &str,
        provider: &str,
    ) -> Option<String> {
        let token = self
            .retired
            .iter()
            .filter(|(_, route)| {
                route.session_id() == session_id
                    && route.owner_subject == owner_subject
                    && route.provider == provider
            })
            .max_by_key(|(_, route)| route.retired_at)
            .map(|(token, _)| token.clone())?;
        self.retired.remove(&token);
        Some(token)
    }
}

#[derive(Clone)]
struct RelayStreamContext {
    session_id: String,
    request_id: String,
}

struct InstrumentedRelayStream {
    upstream: RelayByteStream,
    cancellation: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    context: RelayStreamContext,
    status: StatusCode,
    bytes_received: u64,
    terminated: bool,
}

impl InstrumentedRelayStream {
    fn new(
        upstream: RelayByteStream,
        cancellation: comet_harness::CancellationToken,
        context: RelayStreamContext,
        status: StatusCode,
    ) -> Self {
        Self {
            upstream,
            cancellation: Box::pin(cancellation.cancelled_owned()),
            context,
            status,
            bytes_received: 0,
            terminated: false,
        }
    }

    fn finish(&mut self, outcome: &'static str, error: Option<&(dyn Error + Send + Sync)>) {
        self.terminated = true;
        if let Some(error) = error {
            tracing::warn!(
                outcome,
                session_id = %self.context.session_id,
                request_id = %self.context.request_id,
                status = self.status.as_u16(),
                bytes_received = self.bytes_received,
                err = %error,
                "inference relay response stream terminated"
            );
        } else {
            tracing::debug!(
                outcome,
                session_id = %self.context.session_id,
                request_id = %self.context.request_id,
                status = self.status.as_u16(),
                bytes_received = self.bytes_received,
                "inference relay response stream terminated"
            );
        }
    }
}

impl Stream for InstrumentedRelayStream {
    type Item = Result<Bytes, BoxError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.terminated {
            return Poll::Ready(None);
        }
        if this.cancellation.as_mut().poll(cx).is_ready() {
            this.finish("cancelled", None);
            return Poll::Ready(None);
        }
        match this.upstream.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(bytes))) => {
                this.bytes_received = this.bytes_received.saturating_add(bytes.len() as u64);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.finish("upstream_error", Some(error.as_ref()));
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finish("complete", None);
                Poll::Ready(None)
            }
        }
    }
}

impl Drop for InstrumentedRelayStream {
    fn drop(&mut self) {
        if !self.terminated {
            self.finish("downstream_dropped", None);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn inference_binding(
    harness: HarnessId,
    selected_model: Option<&str>,
) -> Option<(&'static str, String)> {
    let selected = selected_model
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "default")
        .map(str::to_string)
        .or_else(|| match harness {
            HarnessId::ClaudeCode => Some("claude-opus-5".into()),
            HarnessId::Codex => Some("gpt-5.6-sol".into()),
            _ => None,
        })?;
    let lower = selected.to_ascii_lowercase();
    let provider = match harness {
        HarnessId::ClaudeCode if !lower.contains('/') => "anthropic",
        HarnessId::Codex if !lower.contains('/') => "openai",
        _ if lower.starts_with("anthropic/") => "anthropic",
        _ if lower.starts_with("openai/") || lower.starts_with("openai-codex/") => "openai",
        _ => return None,
    };
    let provider_model = selected
        .rsplit_once('/')
        .map(|(_, model)| model)
        .unwrap_or(selected.as_str());
    (!provider_model.is_empty()).then(|| (provider, provider_model.to_string()))
}
/// Agent Auth keys routes by UUID. Imported local transcripts intentionally use
/// `local-chat-*` ids so they never dial an Edge session room; project only that
/// local namespace to a stable RFC 9562 v8 UUID at the control-plane boundary.
fn agent_auth_logical_session_id(session_id: &str) -> Cow<'_, str> {
    if !session_id.starts_with("local-chat-") {
        return Cow::Borrowed(session_id);
    }

    let mut hasher = Sha256::new();
    hasher.update(b"comet-agent-auth-session\0");
    hasher.update(session_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Cow::Owned(uuid::Uuid::from_bytes(bytes).to_string())
}

fn inference_route_request(
    logical_session_id: &str,
    harness: HarnessId,
    selected_model: Option<&str>,
    requested_account_id: Option<&str>,
    lifecycle_epoch: u64,
) -> Result<Option<InferenceRouteRequest>, EngineError> {
    let Some((provider, model)) = inference_binding(harness, selected_model) else {
        return Ok(None);
    };
    let requested_account_id = requested_account_id.map(str::trim).map(str::to_string);
    if requested_account_id.as_deref() == Some("") {
        return Err(EngineError::Other(
            "Agent Auth account selection cannot be empty".into(),
        ));
    }
    Ok(Some(InferenceRouteRequest {
        logical_session_id: logical_session_id.to_string(),
        provider: provider.to_string(),
        model,
        requested_account_id,
        lifecycle_epoch,
    }))
}

fn local_relay_token(provider: &str) -> String {
    let value = format!("{}{}", new_id().replace('-', ""), new_id().replace('-', ""));
    if provider == "anthropic" {
        format!("sk-ant-oat01-{value}")
    } else {
        value
    }
}

impl InferenceRelay {
    pub(crate) fn start(client: ScaffoldClient) -> Result<Self, EngineError> {
        let listener = StdTcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let (route_expired_tx, _) = broadcast::channel(64);
        let relay = Self {
            inner: Arc::new(Inner {
                client,
                port,
                route_state: Mutex::new(RouteState::default()),
                route_expired_tx,
                next_lifecycle_epoch: AtomicU64::new(0),
            }),
        };
        let runtime_listener = TcpListener::from_std(listener)?;
        let serving = relay.clone();
        tokio::spawn(async move { serving.serve(runtime_listener).await });
        Ok(relay)
    }

    pub(crate) fn subscribe_expired_routes(&self) -> broadcast::Receiver<ExpiredRoute> {
        self.inner.route_expired_tx.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn notify_expired_route(&self, logical_session_id: &str, lifecycle_epoch: u64) {
        let _ = self.inner.route_expired_tx.send(ExpiredRoute {
            logical_session_id: logical_session_id.to_string(),
            lifecycle_epoch,
        });
    }

    pub(crate) fn selected_account_id(&self, session_id: &str) -> Option<String> {
        lock(&self.inner.route_state)
            .selected_accounts
            .get(session_id)
            .cloned()
    }

    fn record_selected_account(&self, session_id: &str, account_id: &str) {
        let account_id = account_id.trim();
        if account_id.is_empty() {
            return;
        }
        lock(&self.inner.route_state)
            .selected_accounts
            .insert(session_id.to_string(), account_id.to_string());
    }

    pub(crate) async fn prepare(
        &self,
        session_id: &str,
        harness: HarnessId,
        model: Option<&str>,
        requested_account_id: Option<&str>,
    ) -> Result<Option<InferenceRoute>, EngineError> {
        let lifecycle_epoch = self
            .inner
            .next_lifecycle_epoch
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let agent_auth_session_id = agent_auth_logical_session_id(session_id);
        let local_session_id =
            matches!(&agent_auth_session_id, Cow::Owned(_)).then(|| session_id.to_string());
        let Some(request) = inference_route_request(
            agent_auth_session_id.as_ref(),
            harness,
            model,
            requested_account_id,
            lifecycle_epoch,
        )?
        else {
            tracing::debug!(?harness, "inference relay skipped for unsupported harness");
            return Ok(None);
        };
        let authority = self
            .inner
            .client
            .issue_agent_inference_authority(&comet_harness::CancellationToken::new())
            .await
            .map_err(|error| {
                EngineError::Other(format!("Agent Auth authority issuance failed: {error}"))
            })?;
        let expires_at = validate_authority(&authority)?;
        let provider = request.provider.clone();
        let model = request.model.clone();
        let owner_subject = authority.principal_id.clone();
        let route = Arc::new(Route {
            request,
            local_session_id,
            owner_subject: owner_subject.clone(),
            cancellation: comet_harness::CancellationToken::new(),
            authority: AsyncMutex::new(AuthorityState {
                authority,
                expires_at,
            }),
        });
        let local_token = {
            let mut state = lock(&self.inner.route_state);
            state.prune_retired();
            let token = state
                .take_retired_token(session_id, &owner_subject, &provider)
                .unwrap_or_else(|| local_relay_token(&provider));
            state.active.insert(token.clone(), route);
            token
        };
        Ok(Some(InferenceRoute {
            base_url: format!("http://127.0.0.1:{}", self.inner.port),
            token: local_token,
            provider,
            model,
        }))
    }

    pub(crate) fn remove(&self, local_token: &str) {
        let route = {
            let mut state = lock(&self.inner.route_state);
            state.prune_retired();
            let route = state.active.get(local_token).cloned();
            if let Some(route) = route.as_ref() {
                let aliases = state
                    .active
                    .iter()
                    .filter(|(_, candidate)| Arc::ptr_eq(candidate, route))
                    .map(|(token, _)| token.clone())
                    .collect::<Vec<_>>();
                let retired_at = Instant::now();
                for token in aliases {
                    state.active.remove(&token);
                    state.retired.insert(
                        token,
                        RetiredRoute {
                            logical_session_id: route.request.logical_session_id.clone(),
                            local_session_id: route.local_session_id.clone(),
                            owner_subject: route.owner_subject.clone(),
                            provider: route.request.provider.clone(),
                            lifecycle_epoch: route.request.lifecycle_epoch,
                            retired_at,
                        },
                    );
                }
            }
            route
        };
        if let Some(route) = route {
            route.cancellation.cancel();
        }
    }

    async fn serve(self, listener: TcpListener) {
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(err = %error, "inference relay accept failed");
                    continue;
                }
            };
            if !peer.ip().is_loopback() {
                tracing::warn!(%peer, "inference relay rejected non-loopback peer");
                continue;
            }
            let permit = match permits.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return,
            };
            let relay = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(error) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |request| {
                            let relay = relay.clone();
                            async move { Ok::<_, Infallible>(relay.handle(request).await) }
                        }),
                    )
                    .await
                {
                    tracing::debug!(err = %error, "inference relay connection ended");
                }
            });
        }
    }

    async fn handle(&self, request: Request<Incoming>) -> Response<RelayBody> {
        let token = relay_token(request.headers());
        let (route, retired_session) = {
            let mut state = lock(&self.inner.route_state);
            state.prune_retired();
            let mut route = token
                .as_deref()
                .and_then(|token| state.active.get(token).cloned());
            let retired = if route.is_none() {
                token.as_deref().and_then(|token| {
                    state.retired.get(token).map(|route| {
                        (
                            route.session_id().to_string(),
                            route.owner_subject.clone(),
                            route.provider.clone(),
                            route.lifecycle_epoch,
                        )
                    })
                })
            } else {
                None
            };
            let retired_route =
                if let Some((session_id, owner_subject, provider, lifecycle_epoch)) = retired {
                    let replacement = state
                        .active
                        .values()
                        .find(|candidate| {
                            candidate.session_id() == session_id
                                && candidate.owner_subject == owner_subject
                                && candidate.request.provider == provider
                        })
                        .cloned();
                    if let (Some(local_token), Some(replacement)) = (token.as_ref(), replacement) {
                        state.retired.remove(local_token);
                        state
                            .active
                            .insert(local_token.clone(), replacement.clone());
                        route = Some(replacement);
                        None
                    } else {
                        let restart_required = !state
                            .active
                            .values()
                            .any(|candidate| candidate.session_id() == session_id);
                        Some((session_id, lifecycle_epoch, restart_required))
                    }
                } else {
                    None
                };
            (route, retired_route)
        };
        let Some(route) = route else {
            if let Some((session_id, lifecycle_epoch, restart_required)) = retired_session {
                if restart_required {
                    let _ = self.inner.route_expired_tx.send(ExpiredRoute {
                        logical_session_id: session_id,
                        lifecycle_epoch,
                    });
                }
                return json_response(
                    StatusCode::GONE,
                    json!({ "error": "inference_route_expired", "restart_required": restart_required }),
                );
            }
            return json_response(
                StatusCode::UNAUTHORIZED,
                json!({ "error": "invalid_grant" }),
            );
        };
        let path = request.uri().path();
        if request.method() == Method::GET && path == "/v1/models" {
            return model_catalog(&route.request);
        }
        if request.method() != Method::POST {
            return json_response(
                StatusCode::METHOD_NOT_ALLOWED,
                json!({ "error": "method_not_allowed" }),
            );
        }
        let endpoint = match path {
            "/v1/responses" => "responses",
            "/v1/messages" => "messages",
            _ => return json_response(StatusCode::NOT_FOUND, json!({ "error": "not_found" })),
        };
        let query = match request.uri().query() {
            None => None,
            Some("beta=true") if endpoint == "messages" => Some("beta=true"),
            Some(_) => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "inference_query_invalid" }),
                );
            }
        };
        let expected_provider = if endpoint == "messages" {
            "anthropic"
        } else {
            "openai"
        };
        if route.request.provider != expected_provider {
            return json_response(
                StatusCode::CONFLICT,
                json!({ "error": "inference_provider_mismatch" }),
            );
        }
        let content_length = match request
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(length) => length,
            None => {
                return json_response(
                    StatusCode::LENGTH_REQUIRED,
                    json!({ "error": "content_length_required" }),
                );
            }
        };
        let headers = request.headers().clone();
        let request_id = headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty() && value.len() <= 128)
            .map(str::to_string)
            .unwrap_or_else(new_id);
        let stream_context = RelayStreamContext {
            session_id: route.request.logical_session_id.clone(),
            request_id: request_id.clone(),
        };
        let authority = match self.current_authority(&route).await {
            Ok(authority) => authority,
            Err(error) => {
                tracing::warn!(err = %error, "inference relay authority refresh failed");
                return json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({ "error": "agent_auth_unavailable" }),
                );
            }
        };
        let cancellation = route.cancellation.clone();
        let body = reqwest::Body::wrap_stream(request.into_body().into_data_stream());
        let upstream = self
            .inner
            .client
            .proxy_agent_inference(AgentInferenceProxyRequest {
                endpoint,
                query,
                authority: &authority,
                conversation_id: &route.request.logical_session_id,
                requested_account_id: route.request.requested_account_id.as_deref(),
                request_id: &request_id,
                headers: sanitize_request_headers(headers),
                content_length,
                body,
                cancellation: &cancellation,
            })
            .await;
        match upstream {
            Ok(response) => {
                if let Some(account_id) = response
                    .headers()
                    .get("x-agent-auth-selected-account-id")
                    .and_then(|value| value.to_str().ok())
                {
                    self.record_selected_account(route.session_id(), account_id);
                }
                stream_response(response, cancellation, stream_context)
            }
            Err(error) => {
                tracing::warn!(err = %error, "inference relay upstream failed");
                json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({ "error": "inference_upstream_unavailable" }),
                )
            }
        }
    }

    async fn current_authority(
        &self,
        route: &Route,
    ) -> Result<AgentInferenceAuthority, EngineError> {
        let mut current = route.authority.lock().await;
        if current.expires_at <= Utc::now() + TimeDelta::seconds(REFRESH_SKEW_SECONDS) {
            let authority = self
                .inner
                .client
                .issue_agent_inference_authority(&comet_harness::CancellationToken::new())
                .await
                .map_err(|error| {
                    EngineError::Other(format!("Agent Auth authority renewal failed: {error}"))
                })?;
            let expires_at = validate_authority(&authority)?;
            *current = AuthorityState {
                authority,
                expires_at,
            };
        }
        Ok(current.authority.clone())
    }
}

fn validate_authority(authority: &AgentInferenceAuthority) -> Result<DateTime<Utc>, EngineError> {
    if authority.contract_version != 2
        || authority.token_type != "Bearer"
        || authority.authority_id.is_empty()
        || authority.principal_id.is_empty()
        || authority.authority_scope.is_empty()
        || authority.token.is_empty()
    {
        return Err(EngineError::Other(
            "Agent Auth returned an invalid v2 authority".into(),
        ));
    }
    let expires_at = DateTime::parse_from_rfc3339(&authority.expires_at)
        .map_err(|_| EngineError::Other("Agent Auth returned an invalid authority expiry".into()))?
        .with_timezone(&Utc);
    if expires_at <= Utc::now() {
        return Err(EngineError::Other(
            "Agent Auth returned an expired authority".into(),
        ));
    }
    Ok(expires_at)
}

fn relay_token(headers: &HeaderMap) -> Option<String> {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, token)| {
            scheme.eq_ignore_ascii_case("bearer")
                && !token.is_empty()
                && !token.contains(char::is_whitespace)
        })
        .map(|(_, token)| token);
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|token| !token.is_empty() && !token.contains(char::is_whitespace));
    match (bearer, api_key) {
        (Some(token), None) | (None, Some(token)) => Some(token.to_string()),
        (Some(bearer), Some(api_key)) if bearer == api_key => Some(bearer.to_string()),
        _ => None,
    }
}

fn sanitize_request_headers(mut headers: HeaderMap) -> HeaderMap {
    for name in [AUTHORIZATION, HOST, CONTENT_LENGTH, CONNECTION] {
        headers.remove(name);
    }
    headers.remove("x-api-key");
    for name in [
        "x-agent-auth-owner-subject",
        "x-agent-auth-session-id",
        "x-agent-auth-provider",
        "x-agent-auth-model",
        "x-agent-auth-harness",
        "x-agent-auth-source",
        "x-agent-auth-lifecycle-epoch",
        "x-agent-auth-environment",
        "x-agent-auth-routing-mode",
        "x-agent-auth-requested-account-id",
        "x-agent-auth-account-id",
        "x-agent-auth-conversation-id",
        "x-agent-auth-request-id",
        "x-agent-auth-internal-secret",
    ] {
        headers.remove(name);
    }
    headers
}

fn model_catalog(request: &InferenceRouteRequest) -> Response<RelayBody> {
    let (owned_by, api) = if request.provider == "anthropic" {
        ("anthropic", "anthropic-messages")
    } else {
        ("openai-codex", "openai-codex-responses")
    };
    json_response(
        StatusCode::OK,
        json!({
            "object": "list",
            "data": [{
                "id": format!("{owned_by}/{}", request.model),
                "object": "model",
                "owned_by": owned_by,
                "api": api,
            }],
        }),
    )
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response<RelayBody> {
    let body = Full::new(Bytes::from(body.to_string()))
        .map_err(|never| match never {})
        .boxed_unsync();
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json; charset=utf-8")
        .header("cache-control", "no-store")
        .body(body)
        .expect("static relay response")
}

fn stream_response(
    upstream: reqwest::Response,
    cancellation: comet_harness::CancellationToken,
    context: RelayStreamContext,
) -> Response<RelayBody> {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let upstream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(|error| -> BoxError { Box::new(error) }));
    instrumented_streamed_response(status, headers, Box::pin(upstream), cancellation, context)
}

fn instrumented_streamed_response(
    status: StatusCode,
    headers: HeaderMap,
    stream: RelayByteStream,
    cancellation: comet_harness::CancellationToken,
    context: RelayStreamContext,
) -> Response<RelayBody> {
    let stream = InstrumentedRelayStream::new(stream, cancellation, context, status);
    streamed_response(status, headers, Box::pin(stream))
}

fn streamed_response(
    status: StatusCode,
    headers: HeaderMap,
    stream: RelayByteStream,
) -> Response<RelayBody> {
    let stream = stream.map(|chunk| chunk.map(Frame::data));
    response_with_headers(status, headers, StreamBody::new(stream).boxed_unsync())
}

fn response_with_headers(
    status: StatusCode,
    headers: HeaderMap,
    body: RelayBody,
) -> Response<RelayBody> {
    let mut response = Response::builder().status(status);
    if let Some(target) = response.headers_mut() {
        for (name, value) in headers {
            let Some(name) = name else { continue };
            if !matches!(
                name,
                CONNECTION | hyper::header::TRANSFER_ENCODING | hyper::header::UPGRADE
            ) {
                target.append(name, value);
            }
        }
    }
    response
        .body(body)
        .expect("upstream status and headers are valid")
}

include!("inference_relay_tests.rs");
