use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener as StdTcpListener};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use chrono::{DateTime, TimeDelta, Utc};
use futures::{StreamExt, stream};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{AUTHORIZATION, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderMap};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use tempfile::TempPath;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio_util::io::ReaderStream;

use comet_harness::InferenceRoute;
use comet_proto::{AgentRoutingMode, HarnessId};

use crate::scaffold::{AgentInferenceGrant, AgentInferenceGrantRequest, ScaffoldClient};
use crate::{EngineError, new_id};

const MAX_REQUEST_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;
const REFRESH_SKEW_SECONDS: i64 = 60;
const MAX_ACCOUNT_FAILOVERS: usize = 1;
const MAX_TRANSPORT_REPLAYS: usize = 1;

struct RequestSpool {
    path: TempPath,
    content_length: u64,
}

impl RequestSpool {
    async fn body(&self) -> io::Result<reqwest::Body> {
        let file = tokio::fs::File::open(&self.path).await?;
        Ok(reqwest::Body::wrap_stream(ReaderStream::new(
            file.take(self.content_length),
        )))
    }
}

type BoxError = Box<dyn Error + Send + Sync>;
type RelayBody = UnsyncBoxBody<Bytes, BoxError>;
type RelayByteStream =
    Pin<Box<dyn futures::Stream<Item = Result<Bytes, BoxError>> + Send + 'static>>;

#[derive(Clone)]
pub(crate) struct InferenceRelay {
    inner: Arc<Inner>,
}

struct Inner {
    client: ScaffoldClient,
    port: u16,
    routes: Mutex<HashMap<String, Arc<Route>>>,
    next_lifecycle_epoch: AtomicU64,
}

struct Route {
    request: AgentInferenceGrantRequest,
    grant: AsyncMutex<GrantState>,
    cancellation: comet_harness::CancellationToken,
}

struct GrantState {
    grant: AgentInferenceGrant,
    expires_at: DateTime<Utc>,
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

fn inference_grant_request(
    logical_session_id: &str,
    harness: HarnessId,
    selected_model: Option<&str>,
    requested_account_id: Option<&str>,
    lifecycle_epoch: u64,
) -> Result<Option<AgentInferenceGrantRequest>, EngineError> {
    let Some((provider, model)) = inference_binding(harness, selected_model) else {
        return Ok(None);
    };
    let harness = match harness {
        HarnessId::Codex => "codex",
        HarnessId::ClaudeCode => "claude-code",
        HarnessId::Omp => "omp",
        HarnessId::PrimeAgent => "prime-agent",
        _ => return Ok(None),
    };
    let requested_account_id = requested_account_id.map(str::trim).map(str::to_string);
    if requested_account_id.as_deref() == Some("") {
        return Err(EngineError::Other(
            "Agent Auth account selection cannot be empty".into(),
        ));
    }
    let routing_mode = if requested_account_id.is_some() {
        AgentRoutingMode::Pinned
    } else {
        AgentRoutingMode::Automatic
    };
    Ok(Some(AgentInferenceGrantRequest {
        logical_session_id: logical_session_id.to_string(),
        provider: provider.to_string(),
        model,
        harness: harness.to_string(),
        routing_mode,
        requested_account_id,
        lifecycle_epoch,
    }))
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
                routes: Mutex::new(HashMap::new()),
                next_lifecycle_epoch: AtomicU64::new(0),
            }),
        };
        let runtime_listener = TcpListener::from_std(listener)?;
        let serving = relay.clone();
        tokio::spawn(async move { serving.serve(runtime_listener).await });
        Ok(relay)
    }

    pub(crate) async fn prepare(
        &self,
        logical_session_id: &str,
        harness: HarnessId,
        model: Option<&str>,
        requested_account_id: Option<&str>,
    ) -> Result<Option<InferenceRoute>, EngineError> {
        let lifecycle_epoch = self
            .inner
            .next_lifecycle_epoch
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let Some(request) = inference_grant_request(
            logical_session_id,
            harness,
            model,
            requested_account_id,
            lifecycle_epoch,
        )?
        else {
            tracing::debug!(?harness, "inference relay skipped for unsupported harness");
            return Ok(None);
        };
        let grant = self
            .inner
            .client
            .issue_agent_inference_grant(&request, &comet_harness::CancellationToken::new())
            .await
            .map_err(|error| {
                EngineError::Other(format!("Agent Auth grant issuance failed: {error}"))
            })?;
        let expires_at = validate_grant(&grant, &request)?;
        let local_token = format!("{}{}", new_id().replace('-', ""), new_id().replace('-', ""));
        let route = InferenceRoute {
            base_url: format!("http://127.0.0.1:{}", self.inner.port),
            token: local_token.clone(),
            provider: request.provider.clone(),
            model: request.model.clone(),
        };
        let cancellation = comet_harness::CancellationToken::new();
        lock(&self.inner.routes).insert(
            local_token,
            Arc::new(Route {
                request,
                cancellation,
                grant: AsyncMutex::new(GrantState { grant, expires_at }),
            }),
        );
        Ok(Some(route))
    }

    pub(crate) async fn rebind(&self, logical_session_id: &str) -> Result<(), EngineError> {
        self.inner
            .client
            .rebind_agent_inference_route(
                logical_session_id,
                &comet_harness::CancellationToken::new(),
            )
            .await
            .map_err(|error| EngineError::Other(format!("Agent Auth route rebind failed: {error}")))
    }

    pub(crate) async fn remove(&self, local_token: &str) {
        let route = lock(&self.inner.routes).remove(local_token);
        let Some(route) = route else {
            return;
        };
        route.cancellation.cancel();
        if let Err(error) = self
            .inner
            .client
            .revoke_agent_inference_grants(
                &route.request.logical_session_id,
                &comet_harness::CancellationToken::new(),
            )
            .await
        {
            tracing::warn!(
                session_id = %route.request.logical_session_id,
                err = %error,
                "inference grant revocation failed"
            );
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
        let route = token
            .as_deref()
            .and_then(|token| lock(&self.inner.routes).get(token).cloned());
        let Some(route) = route else {
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
            Some(length) if length <= MAX_REQUEST_BYTES => length,
            Some(_) => {
                return json_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({ "error": "inference_request_too_large" }),
                );
            }
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
        let spool = match spool_request_body(request.into_body(), content_length).await {
            Ok(spool) => spool,
            Err(error) => {
                tracing::warn!(err = %error, "inference relay rejected request body");
                return json_response(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "inference_body_invalid" }),
                );
            }
        };
        let mut grant = match self.current_grant(&route).await {
            Ok(grant) => grant,
            Err(error) => {
                tracing::warn!(err = %error, "inference relay grant refresh failed");
                return json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({ "error": "agent_auth_unavailable" }),
                );
            }
        };
        let cancellation = route.cancellation.clone();
        let sanitized_headers = sanitize_request_headers(headers);
        let mut account_failovers = 0_usize;
        let mut transport_replays = 0_usize;
        loop {
            let body = match spool.body().await {
                Ok(body) => body,
                Err(error) => {
                    tracing::warn!(err = %error, "inference relay could not replay request body");
                    return json_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({ "error": "inference_body_unavailable" }),
                    );
                }
            };
            let upstream = match self
                .inner
                .client
                .proxy_agent_inference(crate::scaffold::AgentInferenceProxyRequest {
                    endpoint,
                    grant: &grant,
                    request_id: &request_id,
                    headers: sanitized_headers.clone(),
                    content_length,
                    body,
                    cancellation: &cancellation,
                })
                .await
            {
                Ok(response) => response,
                Err(error) if transport_replays < MAX_TRANSPORT_REPLAYS => {
                    transport_replays += 1;
                    tracing::warn!(
                        err = %error,
                        attempt = transport_replays,
                        "inference relay upstream failed before response headers; replaying request"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::warn!(err = %error, "inference relay upstream failed");
                    return json_response(
                        StatusCode::BAD_GATEWAY,
                        json!({ "error": "inference_upstream_unavailable" }),
                    );
                }
            };
            if account_failovers >= MAX_ACCOUNT_FAILOVERS {
                return stream_response(upstream, cancellation);
            }
            let (upstream, failure_class) =
                retryable_response(&route.request, &grant, upstream, cancellation.clone()).await;
            let Some(failure_class) = failure_class else {
                return upstream;
            };
            let retry_after_seconds = upstream
                .headers()
                .get(hyper::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let replacement = self
                .inner
                .client
                .report_agent_inference_failure(
                    &grant,
                    failure_class,
                    false,
                    retry_after_seconds,
                    &cancellation,
                )
                .await;
            let replacement = match replacement {
                Ok(Some(replacement)) => replacement,
                Ok(None) => return upstream,
                Err(error) => {
                    tracing::warn!(err = %error, "inference relay failure report was rejected");
                    return upstream;
                }
            };
            if let Err(error) = self
                .replace_current_grant(&route, replacement.clone())
                .await
            {
                tracing::warn!(err = %error, "inference relay replacement grant was invalid");
                return json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({ "error": "agent_auth_unavailable" }),
                );
            }
            drop(upstream);
            account_failovers += 1;
            grant = replacement;
        }
    }

    async fn current_grant(&self, route: &Route) -> Result<AgentInferenceGrant, EngineError> {
        let mut current = route.grant.lock().await;
        if current.expires_at <= Utc::now() + TimeDelta::seconds(REFRESH_SKEW_SECONDS) {
            let refreshed = self
                .inner
                .client
                .issue_agent_inference_grant(
                    &route.request,
                    &comet_harness::CancellationToken::new(),
                )
                .await
                .map_err(|error| {
                    EngineError::Other(format!("Agent Auth grant renewal failed: {error}"))
                })?;
            let expires_at = validate_grant(&refreshed, &route.request)?;
            *current = GrantState {
                grant: refreshed,
                expires_at,
            };
        }
        Ok(current.grant.clone())
    }

    async fn replace_current_grant(
        &self,
        route: &Route,
        grant: AgentInferenceGrant,
    ) -> Result<(), EngineError> {
        let expires_at = validate_grant(&grant, &route.request)?;
        *route.grant.lock().await = GrantState { grant, expires_at };
        Ok(())
    }
}

async fn spool_request_body(body: Incoming, content_length: u64) -> io::Result<RequestSpool> {
    let (file, path) = tokio::task::spawn_blocking(|| -> io::Result<_> {
        Ok(tempfile::NamedTempFile::new()?.into_parts())
    })
    .await
    .map_err(io::Error::other)??;
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = body.into_data_stream();
    let mut observed = 0_u64;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(io::Error::other)?;
        observed = observed.checked_add(bytes.len() as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "inference request length overflow",
            )
        })?;
        if observed > content_length || observed > MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "inference request exceeded declared length",
            ));
        }
        file.write_all(&bytes).await?;
    }
    if observed != content_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "inference request did not match declared length",
        ));
    }
    file.flush().await?;
    drop(file);
    Ok(RequestSpool {
        path,
        content_length,
    })
}

async fn retryable_response(
    request: &AgentInferenceGrantRequest,
    grant: &AgentInferenceGrant,
    upstream: reqwest::Response,
    cancellation: comet_harness::CancellationToken,
) -> (Response<RelayBody>, Option<&'static str>) {
    if request.routing_mode == AgentRoutingMode::Pinned || grant.binding.backend != "oauth" {
        return (stream_response(upstream, cancellation), None);
    }
    if upstream.status() == StatusCode::UNAUTHORIZED {
        return (
            stream_response(upstream, cancellation),
            Some("authentication_required"),
        );
    }
    if upstream.status() != StatusCode::TOO_MANY_REQUESTS
        || upstream
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
        || upstream
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|length| length > MAX_FAILURE_RESPONSE_BYTES)
    {
        return (stream_response(upstream, cancellation), None);
    }

    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut remaining = Box::pin(upstream.bytes_stream());
    let mut chunks = Vec::new();
    let mut observed = 0_usize;
    while let Some(chunk) = remaining.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                let prefix = stream::iter(chunks.into_iter().map(Ok::<Bytes, BoxError>));
                let failure = stream::once(async move { Err::<Bytes, BoxError>(Box::new(error)) });
                return (
                    streamed_response(status, headers, Box::pin(prefix.chain(failure))),
                    None,
                );
            }
        };
        observed = match observed.checked_add(chunk.len()) {
            Some(observed) => observed,
            None => {
                chunks.push(chunk);
                let prefix = stream::iter(chunks.into_iter().map(Ok::<Bytes, BoxError>));
                let rest =
                    remaining.map(|chunk| chunk.map_err(|error| -> BoxError { Box::new(error) }));
                return (
                    streamed_response(status, headers, Box::pin(prefix.chain(rest))),
                    None,
                );
            }
        };
        chunks.push(chunk);
        if observed > MAX_FAILURE_RESPONSE_BYTES {
            let prefix = stream::iter(chunks.into_iter().map(Ok::<Bytes, BoxError>));
            let rest =
                remaining.map(|chunk| chunk.map_err(|error| -> BoxError { Box::new(error) }));
            return (
                streamed_response(status, headers, Box::pin(prefix.chain(rest))),
                None,
            );
        }
    }

    let mut body = Vec::with_capacity(observed);
    for chunk in chunks {
        body.extend_from_slice(&chunk);
    }
    let exhausted = confirmed_account_exhaustion(&body);
    (
        buffered_response(status, headers, Bytes::from(body)),
        exhausted.then_some("account_exhausted"),
    )
}

const MAX_FAILURE_RESPONSE_BYTES: usize = 64 * 1024;
const ACCOUNT_EXHAUSTION_CODES: [&str; 3] = [
    "insufficient_quota",
    "subscription_limit_reached",
    "usage_limit_reached",
];
const ACCOUNT_EXHAUSTION_TYPES: [&str; 2] = ["rate_limit_error", "usage_limit_error"];

fn confirmed_account_exhaustion(body: &[u8]) -> bool {
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let Some(payload) = payload.as_object() else {
        return false;
    };
    let nested = payload.get("error").and_then(serde_json::Value::as_object);
    let code = nested
        .and_then(|error| error.get("code"))
        .or_else(|| payload.get("code"))
        .and_then(serde_json::Value::as_str);
    let failure_type = nested
        .and_then(|error| error.get("type"))
        .or_else(|| payload.get("type"))
        .and_then(serde_json::Value::as_str);
    code.is_some_and(|code| {
        ACCOUNT_EXHAUSTION_CODES
            .iter()
            .any(|expected| code.trim().eq_ignore_ascii_case(expected))
    }) || failure_type.is_some_and(|failure_type| {
        ACCOUNT_EXHAUSTION_TYPES
            .iter()
            .any(|expected| failure_type.trim().eq_ignore_ascii_case(expected))
    })
}

fn validate_grant(
    grant: &AgentInferenceGrant,
    request: &AgentInferenceGrantRequest,
) -> Result<DateTime<Utc>, EngineError> {
    let binding = &grant.binding;
    let pinned_binding_invalid = request.routing_mode == AgentRoutingMode::Pinned
        && (binding.backend != "oauth"
            || binding.account_id.as_deref() != request.requested_account_id.as_deref()
            || binding.account_generation.is_none());
    let automatic_oauth_binding_invalid = request.routing_mode == AgentRoutingMode::Automatic
        && binding.backend == "oauth"
        && (binding.account_id.is_none() || binding.account_generation.is_none());
    if grant.token.is_empty()
        || binding.owner_subject.is_empty()
        || binding.logical_session_id != request.logical_session_id
        || binding.provider != request.provider
        || binding.model != request.model
        || binding.harness != request.harness
        || binding.routing_mode != request.routing_mode
        || binding.requested_account_id != request.requested_account_id
        || binding.source != "comet-local"
        || binding.lifecycle_epoch != request.lifecycle_epoch
        || binding.environment != "local"
        || !matches!(binding.backend.as_str(), "oauth" | "bifrost")
        || pinned_binding_invalid
        || automatic_oauth_binding_invalid
    {
        return Err(EngineError::Other(
            "Agent Auth returned a mismatched inference grant".into(),
        ));
    }
    let expires_at = DateTime::parse_from_rfc3339(&grant.expires_at)
        .map_err(|_| EngineError::Other("Agent Auth returned an invalid grant expiry".into()))?
        .with_timezone(&Utc);
    if expires_at <= Utc::now() {
        return Err(EngineError::Other(
            "Agent Auth returned an expired inference grant".into(),
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
        "x-agent-auth-request-id",
        "x-agent-auth-internal-secret",
    ] {
        headers.remove(name);
    }
    headers
}

fn model_catalog(request: &AgentInferenceGrantRequest) -> Response<RelayBody> {
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
) -> Response<RelayBody> {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(|error| -> BoxError { Box::new(error) }))
        .take_until(cancellation.cancelled_owned());
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

fn buffered_response(status: StatusCode, headers: HeaderMap, body: Bytes) -> Response<RelayBody> {
    let body = Full::new(body)
        .map_err(|never| match never {})
        .boxed_unsync();
    response_with_headers(status, headers, body)
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
