use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener as StdTcpListener};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use chrono::{DateTime, TimeDelta, Utc};
use futures::StreamExt;
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
use comet_proto::HarnessId;

use crate::scaffold::{AgentInferenceGrant, AgentInferenceGrantRequest, ScaffoldClient};
use crate::{EngineError, new_id};

const MAX_REQUEST_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;
const REFRESH_SKEW_SECONDS: i64 = 60;
const MAX_FAILOVER_ATTEMPTS: usize = 4;

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

#[derive(Clone)]
pub(crate) struct InferenceRelay {
    inner: Arc<Inner>,
}

struct Inner {
    client: ScaffoldClient,
    port: u16,
    routes: Mutex<HashMap<String, Arc<Route>>>,
    lifecycle_epochs: Mutex<HashMap<String, u64>>,
}

struct Route {
    request: AgentInferenceGrantRequest,
    grant: AsyncMutex<GrantState>,
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
        .unwrap_or_else(|| match harness {
            HarnessId::ClaudeCode => "claude-opus-5".into(),
            HarnessId::Codex | HarnessId::Omp | HarnessId::PrimeAgent => "gpt-5.6-sol".into(),
            _ => String::new(),
        });
    if selected.is_empty() {
        return None;
    }
    if let Some(model) = selected.strip_prefix("anthropic/") {
        return (!model.is_empty()).then(|| ("anthropic", model.to_string()));
    }
    if let Some(model) = selected.strip_prefix("openai-codex/") {
        return (!model.is_empty()).then(|| ("openai", model.to_string()));
    }
    let provider = if selected.to_ascii_lowercase().contains("claude")
        || selected.to_ascii_lowercase().contains("anthropic")
    {
        "anthropic"
    } else {
        "openai"
    };
    Some((provider, selected))
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
                lifecycle_epochs: Mutex::new(HashMap::new()),
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
    ) -> Result<Option<InferenceRoute>, EngineError> {
        let Some((provider, model)) = inference_binding(harness, model) else {
            return Ok(None);
        };
        let harness = match harness {
            HarnessId::Codex => "codex",
            HarnessId::ClaudeCode => "claude-code",
            HarnessId::Omp => "omp",
            HarnessId::PrimeAgent => "prime-agent",
            other => {
                tracing::debug!(?other, "inference relay skipped for unsupported harness");
                return Ok(None);
            }
        };
        let lifecycle_epoch = lock(&self.inner.lifecycle_epochs)
            .get(logical_session_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        let request = AgentInferenceGrantRequest {
            logical_session_id: logical_session_id.to_string(),
            provider: provider.to_string(),
            model,
            harness: harness.to_string(),
            lifecycle_epoch,
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
        lock(&self.inner.lifecycle_epochs).insert(logical_session_id.to_string(), lifecycle_epoch);
        let local_token = format!("{}{}", new_id().replace('-', ""), new_id().replace('-', ""));
        let route = InferenceRoute {
            base_url: format!("http://127.0.0.1:{}", self.inner.port),
            token: local_token.clone(),
            provider: request.provider.clone(),
            model: request.model.clone(),
        };
        lock(&self.inner.routes).insert(
            local_token,
            Arc::new(Route {
                request,
                grant: AsyncMutex::new(GrantState { grant, expires_at }),
            }),
        );
        Ok(Some(route))
    }

    pub(crate) async fn remove(&self, local_token: &str) {
        let route = lock(&self.inner.routes).remove(local_token);
        let Some(route) = route else {
            return;
        };
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
        let cancellation = comet_harness::CancellationToken::new();
        let sanitized_headers = sanitize_request_headers(headers);
        let mut failovers = 0_usize;
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
                .proxy_agent_inference(
                    endpoint,
                    &grant,
                    &request_id,
                    sanitized_headers.clone(),
                    content_length,
                    body,
                    &cancellation,
                )
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!(err = %error, "inference relay upstream failed");
                    return json_response(
                        StatusCode::BAD_GATEWAY,
                        json!({ "error": "inference_upstream_unavailable" }),
                    );
                }
            };
            let Some(failure_class) = retryable_failure(&grant, upstream.status()) else {
                return stream_response(upstream);
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
                Ok(None) => return stream_response(upstream),
                Err(error) => {
                    tracing::warn!(err = %error, "inference relay failure report was rejected");
                    return stream_response(upstream);
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
            if failovers >= MAX_FAILOVER_ATTEMPTS {
                return stream_response(upstream);
            }
            failovers += 1;
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

fn retryable_failure(grant: &AgentInferenceGrant, status: StatusCode) -> Option<&'static str> {
    if grant.binding.backend != "oauth" {
        return None;
    }
    match status {
        StatusCode::TOO_MANY_REQUESTS => Some("account_exhausted"),
        StatusCode::UNAUTHORIZED => Some("authentication_required"),
        _ => None,
    }
}

fn validate_grant(
    grant: &AgentInferenceGrant,
    request: &AgentInferenceGrantRequest,
) -> Result<DateTime<Utc>, EngineError> {
    let binding = &grant.binding;
    if grant.token.is_empty()
        || binding.owner_subject.is_empty()
        || binding.logical_session_id != request.logical_session_id
        || binding.provider != request.provider
        || binding.model != request.model
        || binding.harness != request.harness
        || binding.source != "comet-local"
        || binding.lifecycle_epoch != request.lifecycle_epoch
        || !matches!(binding.backend.as_str(), "oauth" | "bifrost")
        || (binding.backend == "oauth" && binding.account_id.is_none())
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

fn stream_response(upstream: reqwest::Response) -> Response<RelayBody> {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let stream = upstream.bytes_stream().map(|chunk| {
        chunk
            .map(Frame::data)
            .map_err(|error| -> BoxError { Box::new(error) })
    });
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
        .body(StreamBody::new(stream).boxed_unsync())
        .expect("upstream status and headers are valid")
}

include!("inference_relay_tests.rs");
