//! Crew: the loopback inference relay (docs/crew/zgx-thin-port.md, slice 5),
//! ported from Crew's `crates/engine/src/inference_relay.rs` (Ashler-AI/comet)
//! for Claude Code and Codex. OMP's and Antigravity's transports and Crew's
//! Boat admission gate are not ported yet.
//!
//! A managed child gets an opaque token for this relay and nothing else. The
//! relay answers loopback peers only, a route's token only, and only its
//! provider's endpoint (`/v1/messages` or `/v1/responses`). It forwards to
//! Agent Auth with the shared authority, streaming both ways, and never
//! buffers or replays a model request.
//!
//! - One authority lease serves every route: renewed at expiry, coalesced
//!   across routes, cooled down after a failure, and dropped when the Scaffold
//!   credential changes. A route is bound to the principal and scope it was
//!   prepared under; another one cancels it.
//! - A route is one chat's (its logical session), for one harness, provider and
//!   account. Preparing the same again reuses it, so a parked or warm runtime
//!   keeps its environment; anything else retires the chat's earlier routes.
//!   Upstream has no "run ended" callback, so routes otherwise end at shutdown.
//! - A retired token answers 410 for 30 minutes, unless the chat has a current
//!   route for the same provider, which it then follows (a worker its old
//!   runtime left running).
//! - A 401 saying the authority is invalid drops it for the next request; the
//!   failed request itself is never replayed.

use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener as StdTcpListener};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

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
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use zeron_harness::CancellationToken;
use zeron_harness::crew_inference::Provider;
use zeron_proto::HarnessId;

use super::agent_auth::{
    AGENT_AUTH_HEADERS, AgentAuthClient, AgentAuthError, AgentInferenceAuthority,
    AgentInferenceProxyRequest,
};
use crate::new_id;

#[derive(Clone, Debug, PartialEq)]
struct InferenceRouteRequest {
    harness: HarnessId,
    logical_session_id: String,
    provider: Provider,
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
type RelayByteStream = Pin<Box<dyn futures::Stream<Item = Result<Bytes, BoxError>> + Send + 'static>>;

/// What a managed run is given: the relay, its token, and what it serves.
#[derive(Clone)]
pub(crate) struct PreparedRoute {
    pub base_url: String,
    pub token: String,
    pub provider: Provider,
    pub model: String,
    /// Which route this is: a new one (another account, a replacement) has
    /// another serial.
    pub serial: u64,
}

impl std::fmt::Debug for PreparedRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedRoute")
            .field("base_url", &self.base_url)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("serial", &self.serial)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Why a route couldn't be prepared.
#[derive(Debug)]
pub(crate) enum PrepareError {
    /// The harness, or the model it was asked for, has no managed binding.
    Unsupported,
    /// An account selection that names nothing.
    InvalidAccount,
    /// The relay has shut down ([`InferenceRelay::shutdown`]).
    Stopped,
    /// No authority: signed out, Agent Auth unreachable or refusing.
    Authority(AuthorityRefreshError),
}

#[derive(Clone)]
pub(crate) struct InferenceRelay {
    inner: Arc<Inner>,
}

struct Inner {
    client: AgentAuthClient,
    port: u16,
    route_state: Mutex<RouteState>,
    authority: Arc<AsyncMutex<AuthorityState>>,
    /// Stops accepting connections ([`InferenceRelay::shutdown`]).
    stopped: CancellationToken,
}

struct Route {
    request: InferenceRouteRequest,
    /// The token the route was prepared with (others may alias it).
    token: String,
    /// Original local chat id when Agent Auth needs a UUID projection.
    local_session_id: Option<String>,
    owner_subject: String,
    authority_scope: Option<String>,
    cancellation: CancellationToken,
    serial: u64,
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
pub(crate) struct AuthorityRefreshError {
    pub reason: &'static str,
    pub retry_after: Option<Duration>,
    pub cause: Option<String>,
    /// Nobody is signed in to Crew (or the profile's identity changed).
    pub signed_out: bool,
}

impl AuthorityRefreshError {
    fn new(reason: &'static str) -> Self {
        Self {
            reason,
            retry_after: None,
            cause: None,
            signed_out: false,
        }
    }

    fn client(reason: &'static str, error: &AgentAuthError) -> Self {
        // Response strings are untrusted; the error keeps typed status only.
        let retry_after = error.is_transient().then(|| {
            error
                .retry_after()
                .unwrap_or(Duration::from_secs(1))
                .max(Duration::from_secs(1))
        });
        Self {
            reason,
            retry_after,
            cause: Some(error.to_string()),
            signed_out: matches!(error, AgentAuthError::SignedOut),
        }
    }
}

#[derive(Default)]
struct RouteState {
    active: HashMap<String, Arc<Route>>,
    retired: HashMap<String, RetiredRoute>,
    serial: u64,
}

struct RetiredRoute {
    logical_session_id: String,
    local_session_id: Option<String>,
    owner_subject: String,
    authority_scope: Option<String>,
    provider: Provider,
    retired_at: Instant,
}

impl RetiredRoute {
    fn of(route: &Route, retired_at: Instant) -> Self {
        Self {
            logical_session_id: route.request.logical_session_id.clone(),
            local_session_id: route.local_session_id.clone(),
            owner_subject: route.owner_subject.clone(),
            authority_scope: route.authority_scope.clone(),
            provider: route.request.provider,
            retired_at,
        }
    }

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

    /// Retire every token of each route `pick` selects; the routes come back
    /// to be cancelled once the lock is released.
    fn retire(&mut self, pick: impl Fn(&Route) -> bool) -> Vec<Arc<Route>> {
        let tokens: Vec<String> = self
            .active
            .iter()
            .filter(|(_, route)| pick(route))
            .map(|(token, _)| token.clone())
            .collect();
        let retired_at = Instant::now();
        let mut routes: Vec<Arc<Route>> = Vec::new();
        for token in tokens {
            if let Some(route) = self.active.remove(&token) {
                self.retired.insert(token, RetiredRoute::of(&route, retired_at));
                if !routes.iter().any(|seen| Arc::ptr_eq(seen, &route)) {
                    routes.push(route);
                }
            }
        }
        routes
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
        cancellation: CancellationToken,
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
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The provider and provider model a managed `harness` run of `selected_model`
/// reaches: Claude Code on Anthropic's, Codex on OpenAI's. A run names its
/// model (its harness's own default when its chat names none:
/// `CrewInference::prepare`); no model, or another provider's, has no route.
fn inference_binding(harness: HarnessId, selected_model: Option<&str>) -> Option<(Provider, String)> {
    let provider = Provider::for_harness(harness)?;
    let selected = selected_model
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "default")
        .map(str::to_owned)?;
    let lower = selected.to_ascii_lowercase();
    let own_prefix = match provider {
        Provider::Anthropic => lower.starts_with("anthropic/"),
        Provider::Openai => lower.starts_with("openai/") || lower.starts_with("openai-codex/"),
    };
    if lower.contains('/') && !own_prefix {
        return None;
    }
    let model = selected.rsplit_once('/').map_or(selected.as_str(), |(_, model)| model);
    (!model.is_empty()).then(|| (provider, model.to_owned()))
}

/// Whether a managed `harness` run of `model` has a route: one of its CLI's
/// models that Agent Auth's provider for it serves. What a model list offers.
pub(super) fn routable(harness: HarnessId, model: &str) -> bool {
    inference_binding(harness, Some(model)).is_some()
}

/// Agent Auth keys routes by UUID. A chat id that isn't one (an imported
/// `local-chat-*` transcript, a voice orchestrator's) is projected to a stable
/// RFC 9562 v8 UUID at the boundary, as Crew projects its local imports.
fn agent_auth_logical_session_id(session_id: &str) -> Cow<'_, str> {
    if uuid::Uuid::parse_str(session_id).is_ok_and(|id| id.hyphenated().to_string() == session_id) {
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
) -> Result<InferenceRouteRequest, PrepareError> {
    let (provider, model) =
        inference_binding(harness, selected_model).ok_or(PrepareError::Unsupported)?;
    let requested_account_id = requested_account_id.map(str::trim).map(str::to_owned);
    if requested_account_id.as_deref() == Some("") {
        return Err(PrepareError::InvalidAccount);
    }
    Ok(InferenceRouteRequest {
        harness,
        logical_session_id: logical_session_id.to_owned(),
        provider,
        model,
        requested_account_id,
    })
}

/// An opaque loopback token; Anthropic's is OAuth-shaped, as Claude Code
/// expects of an `ANTHROPIC_AUTH_TOKEN`.
fn local_relay_token(provider: Provider) -> String {
    let value = format!("{}{}", new_id().replace('-', ""), new_id().replace('-', ""));
    match provider {
        Provider::Anthropic => format!("sk-ant-oat01-{value}"),
        Provider::Openai => value,
    }
}

impl InferenceRelay {
    /// Bind a loopback port of the moment's choosing and serve it.
    pub(crate) fn start(client: AgentAuthClient) -> std::io::Result<Self> {
        let listener = StdTcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let relay = Self {
            inner: Arc::new(Inner {
                client,
                port,
                route_state: Mutex::new(RouteState::default()),
                authority: Arc::new(AsyncMutex::new(AuthorityState::default())),
                stopped: CancellationToken::new(),
            }),
        };
        let listener = TcpListener::from_std(listener)?;
        let serving = relay.clone();
        tokio::spawn(async move { serving.serve(listener).await });
        Ok(relay)
    }

    pub(crate) fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.inner.port)
    }

    /// The route a run of `harness` in `session_id` (a chat, or a titling
    /// session) uses for `model` and `requested_account_id` (`None`: Agent
    /// Auth's default selection).
    pub(crate) async fn prepare(
        &self,
        session_id: &str,
        harness: HarnessId,
        model: Option<&str>,
        requested_account_id: Option<&str>,
    ) -> Result<PreparedRoute, PrepareError> {
        if self.inner.stopped.is_cancelled() {
            return Err(PrepareError::Stopped);
        }
        let agent_auth_session_id = agent_auth_logical_session_id(session_id);
        let local_session_id =
            matches!(&agent_auth_session_id, Cow::Owned(_)).then(|| session_id.to_owned());
        let request = inference_route_request(
            agent_auth_session_id.as_ref(),
            harness,
            model,
            requested_account_id,
        )?;
        let lease = self.shared_authority().await.map_err(PrepareError::Authority)?;
        let owner_subject = lease.authority.principal_id.clone();
        let authority_scope = lease.authority.authority_scope.clone();
        let (route, ended) = {
            let mut state = lock(&self.inner.route_state);
            // (A shutdown while the authority was coming.)
            if self.inner.stopped.is_cancelled() {
                return Err(PrepareError::Stopped);
            }
            state.prune_retired();
            let same = state
                .active
                .values()
                .find(|route| {
                    route.session_id() == session_id
                        && route.request.harness == harness
                        && route.request.provider == request.provider
                        && route.request.requested_account_id == request.requested_account_id
                        && route.owner_subject == owner_subject
                        && route.authority_scope == authority_scope
                        && !route.cancellation.is_cancelled()
                })
                .cloned();
            match same {
                Some(route) if route.request.model == request.model => (route, Vec::new()),
                Some(route) => {
                    // Another model on the same route: its token stays.
                    let updated = Arc::new(Route {
                        request: InferenceRouteRequest {
                            model: request.model.clone(),
                            ..route.request.clone()
                        },
                        token: route.token.clone(),
                        local_session_id: route.local_session_id.clone(),
                        owner_subject: route.owner_subject.clone(),
                        authority_scope: route.authority_scope.clone(),
                        cancellation: route.cancellation.clone(),
                        serial: route.serial,
                    });
                    for slot in state.active.values_mut() {
                        if Arc::ptr_eq(slot, &route) {
                            *slot = updated.clone();
                        }
                    }
                    (updated, Vec::new())
                }
                None => {
                    // A chat runs on one route at a time: the ones it ran on
                    // before (another account, provider or principal) end.
                    let ended = state.retire(|route| route.session_id() == session_id);
                    state.serial += 1;
                    let token = local_relay_token(request.provider);
                    let route = Arc::new(Route {
                        request,
                        token: token.clone(),
                        local_session_id,
                        owner_subject,
                        authority_scope,
                        cancellation: CancellationToken::new(),
                        serial: state.serial,
                    });
                    state.active.insert(token, route.clone());
                    (route, ended)
                }
            }
        };
        for old in ended {
            old.cancellation.cancel();
        }
        Ok(PreparedRoute {
            base_url: self.base_url(),
            token: route.token.clone(),
            provider: route.request.provider,
            model: route.request.model.clone(),
            serial: route.serial,
        })
    }

    /// The serial of the route a run of `harness` in `session_id` on
    /// `requested_account_id` would use now, if it has one.
    pub(crate) fn current_serial(
        &self,
        session_id: &str,
        harness: HarnessId,
        requested_account_id: Option<&str>,
    ) -> Option<u64> {
        let provider = Provider::for_harness(harness)?;
        let state = lock(&self.inner.route_state);
        state
            .active
            .values()
            .find(|route| {
                route.session_id() == session_id
                    && route.request.harness == harness
                    && route.request.provider == provider
                    && route.request.requested_account_id.as_deref() == requested_account_id
                    && !route.cancellation.is_cancelled()
            })
            .map(|route| route.serial)
    }

    /// The account and serial of the route a run of `harness` in `session_id`
    /// is on now, whichever account that is.
    pub(crate) fn current_route(&self, session_id: &str, harness: HarnessId) -> Option<(Option<String>, u64)> {
        let state = lock(&self.inner.route_state);
        state
            .active
            .values()
            .find(|route| {
                route.session_id() == session_id
                    && route.request.harness == harness
                    && !route.cancellation.is_cancelled()
            })
            .map(|route| (route.request.requested_account_id.clone(), route.serial))
    }

    /// Retire the route `local_token` belongs to (all its tokens), ending its
    /// streams. (Crew retired a run's route as it ended; upstream has no such
    /// moment, so only tests retire one by hand.)
    #[cfg(test)]
    pub(crate) fn remove(&self, local_token: &str) {
        let ended = {
            let mut state = lock(&self.inner.route_state);
            state.prune_retired();
            let Some(route) = state.active.get(local_token).cloned() else {
                return;
            };
            state.retire(|candidate| std::ptr::eq(candidate, Arc::as_ptr(&route)))
        };
        for route in ended {
            route.cancellation.cancel();
        }
    }

    /// Stop serving: every route retires, its streams end, and no new
    /// connection is taken.
    pub(crate) fn shutdown(&self) {
        self.inner.stopped.cancel();
        let ended = lock(&self.inner.route_state).retire(|_| true);
        for route in ended {
            route.cancellation.cancel();
        }
    }

    async fn serve(self, listener: TcpListener) {
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        loop {
            let (stream, peer) = tokio::select! {
                biased;
                _ = self.inner.stopped.cancelled() => return,
                accepted = listener.accept() => match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(err = %error, "inference relay accept failed");
                        continue;
                    }
                },
            };
            if !peer.ip().is_loopback() {
                tracing::warn!(%peer, "inference relay rejected non-loopback peer");
                continue;
            }
            let permit = tokio::select! {
                biased;
                _ = self.inner.stopped.cancelled() => return,
                permit = permits.clone().acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return,
                },
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
                            route.session_id().to_owned(),
                            route.owner_subject.clone(),
                            route.authority_scope.clone(),
                            route.provider,
                        )
                    })
                })
            } else {
                None
            };
            let retired_route = if let Some((session_id, owner_subject, authority_scope, provider)) = retired {
                let replacement = state
                    .active
                    .values()
                    .find(|candidate| {
                        candidate.session_id() == session_id
                            && candidate.owner_subject == owner_subject
                            && candidate.authority_scope == authority_scope
                            && candidate.request.provider == provider
                    })
                    .cloned();
                if let (Some(local_token), Some(replacement)) = (token.as_ref(), replacement) {
                    state.retired.remove(local_token);
                    state.active.insert(local_token.clone(), replacement.clone());
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
            return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "invalid_grant" }));
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
        let (endpoint, expected_provider) = match path {
            "/v1/responses" => ("responses", Provider::Openai),
            "/v1/messages" => ("messages", Provider::Anthropic),
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
        if route.request.provider != expected_provider {
            return json_response(
                StatusCode::CONFLICT,
                json!({ "error": "inference_provider_mismatch" }),
            );
        }
        let Some(content_length) = request
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return json_response(
                StatusCode::LENGTH_REQUIRED,
                json!({ "error": "content_length_required" }),
            );
        };
        let headers = request.headers().clone();
        let request_id = headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty() && value.len() <= 128)
            .map(str::to_owned)
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
                    response.headers_mut().insert(
                        hyper::header::RETRY_AFTER,
                        hyper::header::HeaderValue::from_str(&seconds.to_string()).expect("integer retry delay"),
                    );
                }
                return response;
            }
        };
        let cancellation = route.cancellation.clone();
        if cancellation.is_cancelled() {
            return json_response(StatusCode::GONE, json!({ "error": "inference_run_retired" }));
        }
        // Streamed, never buffered: the body goes out as it comes in.
        let body = reqwest::Body::wrap_stream(request.into_body().into_data_stream());
        let upstream = self
            .inner
            .client
            .proxy(AgentInferenceProxyRequest {
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
            Ok(response) => self.authority_response(response, &authority, cancellation, stream_context).await,
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
        cancellation: CancellationToken,
        context: RelayStreamContext,
    ) -> Response<RelayBody> {
        const MAX_AUTH_ERROR_BYTES: usize = 4096;
        if response.status() != StatusCode::UNAUTHORIZED
            || response.headers().contains_key("x-agent-auth-selected-account-id")
            || response.content_length().is_some_and(|length| length > MAX_AUTH_ERROR_BYTES as u64)
        {
            return stream_response(response, cancellation, context);
        }
        let status = response.status();
        let headers = response.headers().clone();
        let mut upstream = response
            .bytes_stream()
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
            if chunk.as_ref().is_ok_and(Bytes::is_empty) {
                continue;
            }
            let failed = chunk.is_err();
            length += chunk.as_ref().map_or(0, Bytes::len);
            prefix.push(chunk);
            if failed || length > MAX_AUTH_ERROR_BYTES {
                break false;
            }
        };
        if complete {
            #[derive(serde::Deserialize)]
            struct AuthorityError<'a> {
                error: &'a str,
            }
            let bytes: Cow<'_, [u8]> = match prefix.as_slice() {
                [Ok(bytes)] => Cow::Borrowed(bytes),
                chunks => Cow::Owned(
                    chunks
                        .iter()
                        .filter_map(|chunk| chunk.as_ref().ok())
                        .flat_map(|chunk| chunk.iter().copied())
                        .collect(),
                ),
            };
            if serde_json::from_slice::<AuthorityError<'_>>(&bytes)
                .ok()
                .is_some_and(|body| body.error == "agent_auth_v2_authority_invalid")
            {
                self.invalidate_authority(lease).await;
            }
        }
        // Never replay a model POST or consume/rewrite its response. Only a
        // later, independent request may reissue.
        let stream = futures::stream::iter(prefix).chain(upstream);
        instrumented_streamed_response(status, headers, Box::pin(stream), cancellation, context)
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
        } else {
            None
        };
        if let Some(reason) = reason {
            route.cancellation.cancel();
            return Err(AuthorityRefreshError::new(reason));
        }
        Ok(lease)
    }

    async fn shared_authority(&self) -> Result<Arc<AuthorityLease>, AuthorityRefreshError> {
        // One pool for one Scaffold profile, not per account or provider. A
        // credential rotation discards everything cached.
        let mut current = self.inner.authority.clone().lock_owned().await;
        let credential_generation = self
            .inner
            .client
            .credential()
            .await
            .map_err(|error| AuthorityRefreshError::client("auth_unavailable", &error))?
            .generation;
        if current.credential_generation != Some(credential_generation) {
            current.credential_generation = Some(credential_generation);
            current.lease = None;
            current.retry_at = None;
        }
        if let Some(remaining) = current
            .retry_at
            .map(|(started, delay)| delay.saturating_sub(started.elapsed()))
            .filter(|remaining| !remaining.is_zero())
        {
            return Err(AuthorityRefreshError {
                reason: "authority_issuance_cooldown",
                retry_after: Some(remaining),
                cause: None,
                signed_out: false,
            });
        }
        if let Some(lease) = current.lease.as_ref()
            && lease.expires_at > Utc::now() + TimeDelta::seconds(EXPIRY_BOUNDARY_SECONDS)
        {
            return Ok(lease.clone());
        }
        // Only misses spawn work. The owned lock coalesces routes, and
        // cancelling a waiter can't abort another run's acquisition.
        let client = self.inner.client.clone();
        tokio::spawn(async move {
            let result = async {
                if let Some(lease) = current.lease.as_ref() {
                    let now = Utc::now();
                    if lease.expires_at > now {
                        let remaining = (lease.expires_at - now)
                            .to_std()
                            .map_err(|_| AuthorityRefreshError::new("invalid_expiry"))?;
                        tokio::time::sleep(remaining).await;
                        if lease.expires_at > Utc::now() {
                            return Err(AuthorityRefreshError {
                                reason: "expiry_boundary_pending",
                                retry_after: Some(Duration::from_secs(1)),
                                cause: None,
                                signed_out: false,
                            });
                        }
                    }
                }
                let before = client
                    .credential()
                    .await
                    .map_err(|error| AuthorityRefreshError::client("auth_unavailable", &error))?
                    .generation;
                if before != credential_generation {
                    return Err(AuthorityRefreshError::new("credential_changed"));
                }
                let authority = client
                    .issue_authority(&CancellationToken::new())
                    .await
                    .map_err(|error| AuthorityRefreshError::client("authority_issuance_failed", &error))?;
                let expires_at = validate_authority(&authority).map_err(|error| AuthorityRefreshError {
                    reason: "invalid_authority",
                    retry_after: None,
                    cause: Some(error.to_owned()),
                    signed_out: false,
                })?;
                let after = client
                    .credential()
                    .await
                    .map_err(|error| AuthorityRefreshError::client("auth_unavailable", &error))?
                    .generation;
                if after != credential_generation {
                    return Err(AuthorityRefreshError::new("credential_changed"));
                }
                current.generation += 1;
                Ok(Arc::new(AuthorityLease {
                    authority,
                    expires_at,
                    credential_generation,
                    generation: current.generation,
                }))
            }
            .await;
            match &result {
                Ok(lease) => {
                    current.lease = Some(lease.clone());
                    current.retry_at = None;
                }
                Err(error) => {
                    // Coalesce failures without poisoning a credential for good.
                    current.retry_at = Some((
                        Instant::now(),
                        error.retry_after.unwrap_or(Duration::from_secs(1)),
                    ));
                }
            }
            result
        })
        .await
        .map_err(|_| AuthorityRefreshError::new("authority_acquisition_cancelled"))?
    }
}

fn validate_authority(authority: &AgentInferenceAuthority) -> Result<DateTime<Utc>, &'static str> {
    if authority.contract_version != 2
        || authority.token_type != "Bearer"
        || authority.authority_id.is_empty()
        || authority.principal_id.is_empty()
        || authority.authority_scope.as_deref().is_some_and(str::is_empty)
        || authority.token.is_empty()
    {
        return Err("Agent Auth returned an invalid v2 authority");
    }
    let expires_at = DateTime::parse_from_rfc3339(&authority.expires_at)
        .map_err(|_| "Agent Auth returned an invalid authority expiry")?
        .with_timezone(&Utc);
    if expires_at <= Utc::now() {
        return Err("Agent Auth returned an expired authority");
    }
    Ok(expires_at)
}

/// The route token a request presents: a bearer or an `x-api-key`, and when
/// both, the same one.
fn relay_token(headers: &HeaderMap) -> Option<String> {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, token)| {
            scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && !token.contains(char::is_whitespace)
        })
        .map(|(_, token)| token);
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|token| !token.is_empty() && !token.contains(char::is_whitespace));
    let mut tokens = [bearer, api_key].into_iter().flatten();
    let token = tokens.next()?;
    tokens.all(|candidate| candidate == token).then(|| token.to_owned())
}

fn sanitize_request_headers(mut headers: HeaderMap) -> HeaderMap {
    for name in [AUTHORIZATION, HOST, CONTENT_LENGTH, CONNECTION] {
        headers.remove(name);
    }
    for name in AGENT_AUTH_HEADERS {
        headers.remove(*name);
    }
    headers
}

fn model_catalog(request: &InferenceRouteRequest) -> Response<RelayBody> {
    let (owned_by, api) = match request.provider {
        Provider::Anthropic => ("anthropic", "anthropic-messages"),
        Provider::Openai => ("openai-codex", "openai-codex-responses"),
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
    cancellation: CancellationToken,
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
    upstream: RelayByteStream,
    cancellation: CancellationToken,
    context: RelayStreamContext,
) -> Response<RelayBody> {
    let stream = InstrumentedRelayStream::new(upstream, cancellation, context, status);
    streamed_response(status, headers, Box::pin(stream))
}

fn streamed_response(status: StatusCode, headers: HeaderMap, stream: RelayByteStream) -> Response<RelayBody> {
    let stream = stream.map(|chunk| chunk.map(Frame::data));
    let mut response = Response::builder().status(status);
    if let Some(target) = response.headers_mut() {
        for (name, value) in headers {
            let Some(name) = name else { continue };
            if !matches!(name, CONNECTION | hyper::header::TRANSFER_ENCODING | hyper::header::UPGRADE) {
                target.append(name, value);
            }
        }
    }
    response
        .body(StreamBody::new(stream).boxed_unsync())
        .expect("upstream status and headers are valid")
}

include!("crew_inference_relay_tests.rs");
