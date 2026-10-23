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
use std::sync::Arc;
use parking_lot::{Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use zeron_harness::InferenceRoute;
use zeron_proto::HarnessId;

use crate::scaffold::{AgentInferenceAuthority, AgentInferenceProxyRequest, ScaffoldClient};
use crate::{EngineError, new_id};

#[derive(Clone)]
struct InferenceRouteRequest {
    logical_session_id: String,
    provider: String,
    model: String,
    requested_account_id: Option<String>,
}

const MAX_CONNECTIONS: usize = 64;
// Issuer reuse ends at millisecond expiry, while signed tokens expire at whole
// seconds. Wait out the final second before asking for the next authority.
const EXPIRY_BOUNDARY_SECONDS: i64 = 1;
const RETIRED_ROUTE_TTL: Duration = Duration::from_secs(30 * 60);

type BoxError = Box<dyn Error + Send + Sync>;
type RelayBody = UnsyncBoxBody<Bytes, BoxError>;
type RelayByteStream =
    Pin<Box<dyn futures::Stream<Item = Result<Bytes, BoxError>> + Send + 'static>>;


/// A native run's provider protection gate. Metadata/catalog reads never call it.
pub(crate) struct RunInferenceAdmission {
    pub(crate) cancellation: zeron_harness::CancellationToken,
    pub(crate) check: Arc<dyn Fn() -> futures::future::BoxFuture<'static,
        Result<crate::sessions::InferenceActivity, EngineError>> + Send + Sync>,
}
#[derive(Clone)]
pub(crate) struct InferenceRelay {
    inner: Arc<Inner>,
}

struct Inner {
    client: ScaffoldClient,
    port: u16,
    route_state: Mutex<RouteState>,
    authority: Arc<AsyncMutex<AuthorityState>>,
}

struct Route {
    request: InferenceRouteRequest,
    /// Original local chat id when Agent Auth needs a UUID projection.
    local_session_id: Option<String>,
    owner_subject: String,
    authority_scope: Option<String>,
    cancellation: zeron_harness::CancellationToken,
    admission: Option<RunInferenceAdmission>,
}

impl Route {
    fn session_id(&self) -> &str {
        self.local_session_id
            .as_deref()
            .unwrap_or(&self.request.logical_session_id)
    }
}

#[derive(Default)]
struct AuthorityState {
    credential_generation: Option<u64>,
    generation: u64,
    lease: Option<Arc<AuthorityLease>>,
    retry_at: Option<(Instant, Duration)>,
}

struct AuthorityLease {
    authority: AgentInferenceAuthority,
    expires_at: DateTime<Utc>,
    credential_generation: u64,
    generation: u64,
}

#[derive(Debug)]
struct AuthorityRefreshError {
    reason: &'static str,
    retry_after: Option<Duration>,
    cause: Option<String>,
}

impl AuthorityRefreshError {
    fn new(reason: &'static str) -> Self {
        Self { reason, retry_after: None, cause: None }
    }

    fn scaffold(reason: &'static str, error: &crate::scaffold::ScaffoldError) -> Self {
        // API response strings are untrusted; retain only typed status here.
        use crate::scaffold::ScaffoldError;
        let cause = match error {
            ScaffoldError::Api(error) => format!("scaffold_api_error:{}", error.0.status),
            ScaffoldError::InvalidResponse(_) => "scaffold_response_invalid".into(),
            _ => error.to_string(),
        };
        let retry_after = error.is_transient().then(|| {
            error.retry_after().unwrap_or(Duration::from_secs(1)).max(Duration::from_secs(1))
        });
        Self { reason, retry_after, cause: Some(cause) }
    }
}

#[derive(Default)]
struct RouteState {
    active: HashMap<String, Arc<Route>>,
    retired: HashMap<String, RetiredRoute>,
}

struct RetiredRoute {
    logical_session_id: String,
    local_session_id: Option<String>,
    owner_subject: String,
    authority_scope: Option<String>,
    provider: String,
    retired_at: Instant,
    run_bound: bool,
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
        authority_scope: Option<&str>,
        provider: &str,
    ) -> Option<String> {
        let token = self
            .retired
            .iter()
            .filter(|(_, route)| {
                route.session_id() == session_id
                    && route.owner_subject == owner_subject
                    && route.authority_scope.as_deref() == authority_scope
                    && route.provider == provider
                    && !route.run_bound
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
    activity: Option<crate::sessions::InferenceActivity>,
}

impl InstrumentedRelayStream {
    fn new(
        upstream: RelayByteStream,
        cancellation: zeron_harness::CancellationToken,
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
            activity: None,
        }
    }

    fn finish(&mut self, outcome: &'static str, error: Option<&(dyn Error + Send + Sync)>) {
        self.terminated = true;
        self.activity.take();
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
    mutex.lock()
}

fn inference_binding(
    harness: HarnessId,
    selected_model: Option<&str>,
) -> Option<(&'static str, String)> {
    // ACP agents own their CLI credentials. A provider-prefixed model id is
    // not evidence that they implement Crew's Agent Auth relay contract.
    if !matches!(harness, HarnessId::Omp | HarnessId::ClaudeCode | HarnessId::Codex) {
        return None;
    }
    let selected = selected_model
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "default")
        .map(str::to_string)
        .or_else(|| match harness {
            HarnessId::ClaudeCode => Some("claude-opus-5".into()),
            HarnessId::Codex => Some("gpt-6-sol".into()),
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
        let relay = Self {
            inner: Arc::new(Inner {
                client,
                port,
                route_state: Mutex::new(RouteState::default()),
                authority: Arc::new(AsyncMutex::new(AuthorityState::default())),
            }),
        };
        let runtime_listener = TcpListener::from_std(listener)?;
        let serving = relay.clone();
        tokio::spawn(async move { serving.serve(runtime_listener).await });
        Ok(relay)
    }

    #[cfg(test)]
    async fn prepare(&self, session_id: &str, harness: HarnessId, model: Option<&str>,
        requested_account_id: Option<&str>) -> Result<Option<InferenceRoute>, EngineError> {
        self.prepare_with_admission(session_id, harness, model, requested_account_id, None).await
    }

    pub(crate) async fn prepare_with_admission(
        &self,
        session_id: &str,
        harness: HarnessId,
        model: Option<&str>,
        requested_account_id: Option<&str>,
        admission: Option<RunInferenceAdmission>,
    ) -> Result<Option<InferenceRoute>, EngineError> {
        let agent_auth_session_id = agent_auth_logical_session_id(session_id);
        let local_session_id =
            matches!(&agent_auth_session_id, Cow::Owned(_)).then(|| session_id.to_string());
        let Some(request) = inference_route_request(
            agent_auth_session_id.as_ref(),
            harness,
            model,
            requested_account_id,
        )?
        else {
            tracing::debug!(?harness, "inference relay skipped for unsupported harness");
            return Ok(None);
        };
        let cancellation = admission.as_ref().map(|gate| gate.cancellation.child_token())
            .unwrap_or_default();
        let lease = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(EngineError::Other("Agent Auth route cancelled".into())),
            lease = self.shared_authority() => lease.map_err(|error| {
                EngineError::Other(format!("Agent Auth authority issuance failed: {}", error.reason))
            })?,
        };
        let provider = request.provider.clone();
        let model = request.model.clone();
        let owner_subject = lease.authority.principal_id.clone();
        let authority_scope = lease.authority.authority_scope.clone();
        let run_bound = admission.is_some();
        let route = Arc::new(Route {
            request,
            local_session_id,
            owner_subject: owner_subject.clone(),
            cancellation,
            admission,
            authority_scope: authority_scope.clone(),
        });
        let local_token = {
            let mut state = lock(&self.inner.route_state);
            state.prune_retired();
            let token = if run_bound { None } else {
                state.take_retired_token(session_id, &owner_subject, authority_scope.as_deref(), &provider)
            }.unwrap_or_else(|| local_relay_token(&provider));
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
                            authority_scope: route.authority_scope.clone(),
                            provider: route.request.provider.clone(),
                            retired_at,
                            run_bound: route.admission.is_some(),
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
                            route.authority_scope.clone(),
                            route.provider.clone(),
                            route.run_bound,
                        )
                    })
                })
            } else {
                None
            };
            let retired_route =
                if let Some((session_id, owner_subject, authority_scope, provider, run_bound)) = retired {
                    let replacement = state
                        .active
                        .values()
                        .find(|candidate| {
                            candidate.session_id() == session_id
                                && candidate.owner_subject == owner_subject
                                && candidate.authority_scope == authority_scope
                                && candidate.request.provider == provider
                                && !run_bound && candidate.admission.is_none()
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
                        Some(restart_required)
                    }
                } else {
                    None
                };
            (route, retired_route)
        };
        let Some(route) = route else {
            if let Some(restart_required) = retired_session {
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
                tracing::warn!(reason = error.reason, cause = error.cause.as_deref(),
                    "inference relay authority refresh failed");
                let mut response = json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({ "error": "agent_auth_unavailable" }),
                );
                if let Some(delay) = error.retry_after {
                    let seconds = delay.as_secs().saturating_add(u64::from(delay.subsec_nanos() > 0));
                    response.headers_mut().insert(hyper::header::RETRY_AFTER,
                        hyper::header::HeaderValue::from_str(&seconds.to_string()).expect("integer retry delay"));
                }
                return response;
            }
        };
        let cancellation = route.cancellation.clone();
        // This is the last admission boundary before any model request/body is
        // forwarded. A bound route cannot borrow another native run's lease.
        let activity = if let Some(admission) = &route.admission {
            let admitted = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return json_response(StatusCode::GONE,
                    json!({ "error": "inference_run_retired" })),
                result = (admission.check)() => result,
            };
            match admitted {
                Ok(activity) => Some(activity),
                Err(_) => return json_response(StatusCode::SERVICE_UNAVAILABLE,
                    json!({ "error": "provider_protection_unavailable" })),
            }
        } else { None };
        if cancellation.is_cancelled() {
            return json_response(StatusCode::GONE, json!({ "error": "inference_run_retired" }));
        }
        let body = reqwest::Body::wrap_stream(request.into_body().into_data_stream());
        let upstream = self
            .inner
            .client
            .proxy_agent_inference(AgentInferenceProxyRequest {
                endpoint,
                query,
                authority: &authority.authority,
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
            Ok(response) => self.authority_response(response, &authority, cancellation, stream_context, activity).await,
            Err(error) => {
                tracing::warn!(err = %error, "inference relay upstream failed");
                json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({ "error": "inference_upstream_unavailable" }),
                )
            }
        }
    }

    async fn authority_response(
        &self,
        response: reqwest::Response,
        lease: &AuthorityLease,
        cancellation: zeron_harness::CancellationToken,
        context: RelayStreamContext,
        activity: Option<crate::sessions::InferenceActivity>,
    ) -> Response<RelayBody> {
        const MAX_AUTH_ERROR_BYTES: usize = 4096;
        if response.status() != StatusCode::UNAUTHORIZED
            || response.headers().contains_key("x-agent-auth-selected-account-id")
            || response.content_length().is_some_and(|length| length > MAX_AUTH_ERROR_BYTES as u64)
        {
            return stream_response(response, cancellation, context, activity);
        }
        let status = response.status();
        let headers = response.headers().clone();
        let mut upstream = response.bytes_stream()
            .map(|chunk| chunk.map_err(|error| -> BoxError { Box::new(error) }));
        let mut prefix = Vec::new();
        let mut length = 0;
        let deadline = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(deadline);
        let complete = loop {
            let chunk = tokio::select! {
                biased;
                _ = cancellation.cancelled() => break false,
                _ = &mut deadline => break false,
                chunk = upstream.next() => chunk,
            };
            let Some(chunk) = chunk else { break true };
            if chunk.as_ref().is_ok_and(Bytes::is_empty) { continue; }
            let failed = chunk.is_err();
            length += chunk.as_ref().map_or(0, Bytes::len);
            prefix.push(chunk);
            if failed || length > MAX_AUTH_ERROR_BYTES { break false; }
        };
        if complete {
            #[derive(serde::Deserialize)]
            struct AuthorityError<'a> { error: &'a str }
            let bytes: Cow<'_, [u8]> = match prefix.as_slice() {
                [Ok(bytes)] => Cow::Borrowed(bytes),
                chunks => Cow::Owned(chunks.iter().filter_map(|chunk| chunk.as_ref().ok())
                    .flat_map(|chunk| chunk.iter().copied()).collect()),
            };
            if serde_json::from_slice::<AuthorityError<'_>>(&bytes).ok()
                .is_some_and(|body| body.error == "agent_auth_v2_authority_invalid")
            {
                self.invalidate_authority(lease).await;
            }
        }
        // Never replay a model POST or consume/rewrite its response. Only a later
        // independent request may reissue; retain the native activity until EOF.
        let stream = futures::stream::iter(prefix).chain(upstream);
        instrumented_streamed_response(status, headers, Box::pin(stream), cancellation, context, activity)
    }

    async fn invalidate_authority(&self, rejected: &AuthorityLease) {
        let mut current = self.inner.authority.lock().await;
        if current.lease.as_ref().is_some_and(|lease| {
            lease.generation == rejected.generation
                && lease.credential_generation == rejected.credential_generation
        }) {
            current.lease = None;
            current.retry_at = None;
        }
    }

    async fn current_authority(&self, route: &Route) -> Result<Arc<AuthorityLease>, AuthorityRefreshError> {
        let lease = tokio::select! {
            biased;
            _ = route.cancellation.cancelled() => return Err(AuthorityRefreshError::new("route_cancelled")),
            lease = self.shared_authority() => lease?,
        };
        let reason = if lease.authority.principal_id != route.owner_subject {
            Some("route_owner_changed")
        } else if lease.authority.authority_scope != route.authority_scope {
            Some("route_scope_changed")
        } else { None };
        if let Some(reason) = reason {
            route.cancellation.cancel();
            return Err(AuthorityRefreshError::new(reason));
        }
        Ok(lease)
    }

    async fn shared_authority(&self) -> Result<Arc<AuthorityLease>, AuthorityRefreshError> {
        // This pool belongs to one immutable Scaffold client/profile, not an
        // account/provider. Credential rotation discards all cached authorization.
        let mut current = self.inner.authority.clone().lock_owned().await;
        let credential_generation = self.inner.client.credential().await
            .map_err(|error| AuthorityRefreshError::scaffold("auth_unavailable", &error))?.generation;
        if current.credential_generation != Some(credential_generation) {
            current.credential_generation = Some(credential_generation);
            current.lease = None;
            current.retry_at = None;
        }
        if let Some(remaining) = current.retry_at
            .map(|(started, delay)| delay.saturating_sub(started.elapsed()))
            .filter(|remaining| !remaining.is_zero())
        {
            return Err(AuthorityRefreshError {
                reason: "authority_issuance_cooldown", retry_after: Some(remaining), cause: None,
            });
        }
        if let Some(lease) = current.lease.as_ref() {
            if lease.expires_at > Utc::now() + TimeDelta::seconds(EXPIRY_BOUNDARY_SECONDS) {
                return Ok(lease.clone());
            }
        }
        // Only misses spawn work. The owned lock coalesces routes, and cancelling
        // a waiter cannot abort another run's acquisition or take its admission.
        let client = self.inner.client.clone();
        tokio::spawn(async move {
            let result = async {
                if let Some(lease) = current.lease.as_ref() {
                    let now = Utc::now();
                    if lease.expires_at > now {
                        let remaining = (lease.expires_at - now).to_std()
                            .map_err(|_| AuthorityRefreshError::new("invalid_expiry"))?;
                        tokio::time::sleep(remaining).await;
                        if lease.expires_at > Utc::now() {
                            return Err(AuthorityRefreshError {
                                reason: "expiry_boundary_pending",
                                retry_after: Some(Duration::from_secs(1)), cause: None,
                            });
                        }
                    }
                }
                let before = client.credential().await
                    .map_err(|error| AuthorityRefreshError::scaffold("auth_unavailable", &error))?.generation;
                if before != credential_generation {
                    return Err(AuthorityRefreshError::new("credential_changed"));
                }
                let authority = client.issue_agent_inference_authority(&zeron_harness::CancellationToken::new())
                    .await.map_err(|error| AuthorityRefreshError::scaffold("authority_issuance_failed", &error))?;
                let expires_at = validate_authority(&authority).map_err(|error| AuthorityRefreshError {
                    reason: "invalid_authority", retry_after: None, cause: Some(error.to_string()),
                })?;
                let after = client.credential().await
                    .map_err(|error| AuthorityRefreshError::scaffold("auth_unavailable", &error))?.generation;
                if after != credential_generation {
                    return Err(AuthorityRefreshError::new("credential_changed"));
                }
                current.generation += 1;
                Ok(Arc::new(AuthorityLease {
                    authority, expires_at, credential_generation, generation: current.generation,
                }))
            }.await;
            match &result {
                Ok(lease) => {
                    current.lease = Some(lease.clone());
                    current.retry_at = None;
                }
                Err(error) => {
                    // Coalesce failures without permanently poisoning a credential.
                    current.retry_at = Some((Instant::now(), error.retry_after.unwrap_or(Duration::from_secs(1))));
                }
            }
            result
        }).await.map_err(|_| AuthorityRefreshError::new("authority_acquisition_cancelled"))?
    }
}

fn validate_authority(authority: &AgentInferenceAuthority) -> Result<DateTime<Utc>, EngineError> {
    if authority.contract_version != 2
        || authority.token_type != "Bearer"
        || authority.authority_id.is_empty()
        || authority.principal_id.is_empty()
        || authority
            .authority_scope
            .as_deref()
            .is_some_and(str::is_empty)
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
    cancellation: zeron_harness::CancellationToken,
    context: RelayStreamContext,
    activity: Option<crate::sessions::InferenceActivity>,
) -> Response<RelayBody> {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let upstream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(|error| -> BoxError { Box::new(error) }));
    instrumented_streamed_response(status, headers, Box::pin(upstream), cancellation, context, activity)
}

fn instrumented_streamed_response(
    status: StatusCode,
    headers: HeaderMap,
    upstream: RelayByteStream,
    cancellation: zeron_harness::CancellationToken,
    context: RelayStreamContext,
    activity: Option<crate::sessions::InferenceActivity>,
) -> Response<RelayBody> {
    let mut stream = InstrumentedRelayStream::new(upstream, cancellation, context, status);
    stream.activity = activity;
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
