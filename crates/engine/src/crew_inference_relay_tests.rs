// Crew: the relay's behavioural tests, ported from Crew's
// `inference_relay_tests.rs`. OMP, Antigravity and Boat admission cases wait
// for those ports; the route-lifecycle cases follow upstream's flow (routes
// reused per chat and account, retired on replacement or shutdown).
#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream;
    use http_body_util::{BodyExt, Full, StreamBody};
    use hyper::{body::Frame, body::Incoming, service::service_fn};
    use serde_json::Value;
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tokio::sync::mpsc;

    /// A run names its model (`CrewInference::prepare` gives one that names
    /// none its harness's own default): these stand for any.
    const CODEX_MODEL: &str = "gpt-6-sol";
    const CLAUDE_MODEL: &str = "claude-opus-5-5";

    #[derive(Clone)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn relay_stream_context(request_id: &str) -> RelayStreamContext {
        RelayStreamContext {
            session_id: "diagnostic-session".into(),
            request_id: request_id.into(),
        }
    }

    fn instrumented_test_stream(
        items: Vec<Result<Bytes, BoxError>>,
        cancellation: CancellationToken,
        request_id: &str,
    ) -> InstrumentedRelayStream {
        InstrumentedRelayStream::new(
            Box::pin(stream::iter(items)),
            cancellation,
            relay_stream_context(request_id),
            StatusCode::OK,
        )
    }

    pub(crate) fn static_token() -> Arc<dyn zeron_rpc::TokenSource> {
        Arc::new(zeron_rpc::StaticToken("comet-access-token".into()))
    }

    pub(crate) fn start_relay(origin: &str, token: Arc<dyn zeron_rpc::TokenSource>) -> InferenceRelay {
        InferenceRelay::start(AgentAuthClient::new(origin, token).unwrap()).unwrap()
    }

    fn route_of(relay: &InferenceRelay, token: &str) -> Arc<Route> {
        lock(&relay.inner.route_state).active.get(token).unwrap().clone()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn logs_every_relay_stream_termination_with_request_context() {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer({
                let logs = logs.clone();
                move || LogWriter(logs.clone())
            })
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);

        let mut complete = instrumented_test_stream(
            vec![Ok(Bytes::from_static(b"abc"))],
            CancellationToken::new(),
            "request-complete",
        );
        assert_eq!(complete.next().await.unwrap().unwrap(), Bytes::from_static(b"abc"));
        assert!(complete.next().await.is_none());

        let cancellation = CancellationToken::new();
        let mut cancelled = InstrumentedRelayStream::new(
            Box::pin(stream::pending()),
            cancellation.clone(),
            relay_stream_context("request-cancelled"),
            StatusCode::OK,
        );
        cancellation.cancel();
        assert!(cancelled.next().await.is_none());

        let mut failed = instrumented_test_stream(
            vec![Err(Box::new(std::io::Error::other("body closed")))],
            CancellationToken::new(),
            "request-failed",
        );
        assert_eq!(failed.next().await.unwrap().unwrap_err().to_string(), "body closed");

        drop(InstrumentedRelayStream::new(
            Box::pin(stream::pending()),
            CancellationToken::new(),
            relay_stream_context("request-dropped"),
            StatusCode::OK,
        ));
        let logs = String::from_utf8(lock(&logs).clone()).unwrap();
        for (request_id, outcome) in [
            ("request-complete", "complete"),
            ("request-cancelled", "cancelled"),
            ("request-failed", "upstream_error"),
            ("request-dropped", "downstream_dropped"),
        ] {
            let line = logs
                .lines()
                .find(|line| line.contains(request_id))
                .unwrap_or_else(|| panic!("missing diagnostic for {request_id}: {logs}"));
            assert!(line.contains(&format!("outcome=\"{outcome}\"")), "{line}");
            assert!(line.contains("session_id=diagnostic-session"), "{line}");
            assert!(line.contains("status=200"), "{line}");
            assert!(line.contains("bytes_received="), "{line}");
        }
        assert!(logs.lines().any(|line| line.contains("request-failed") && line.contains("body closed")));
    }

    /// A Scaffold credential that changes (another sign-in) or goes away.
    #[derive(Default)]
    pub(crate) struct ProfileToken {
        pub signed_out: AtomicBool,
        pub generation: AtomicU64,
    }

    #[async_trait]
    impl zeron_rpc::TokenSource for ProfileToken {
        async fn token(&self) -> Result<String, zeron_rpc::TokenError> {
            if self.signed_out.load(Ordering::SeqCst) {
                Err(zeron_rpc::TokenError::SignedOut)
            } else {
                Ok(format!("comet-access-token-{}", self.generation.load(Ordering::SeqCst)))
            }
        }
    }

    // These local responses exercise scheduling, not cryptographic renewal.
    pub(crate) struct AuthorityFixture {
        pub expires_at: Mutex<DateTime<Utc>>,
        pub issued: AtomicU64,
        pub forwarded: AtomicU64,
        pub fail_issuance: AtomicBool,
        pub issuance_gate: Mutex<Option<Arc<Semaphore>>>,
        pub principal: Mutex<String>,
        pub scope: Mutex<String>,
        pub rejection: Mutex<Option<(String, bool)>>,
        pub response_gate: Mutex<Option<Arc<Semaphore>>>,
        /// `GET /api/agent-accounts` answers.
        pub accounts_listed: AtomicU64,
    }

    impl AuthorityFixture {
        pub(crate) fn new(expires_at: DateTime<Utc>) -> Arc<Self> {
            Arc::new(Self {
                expires_at: Mutex::new(expires_at),
                issued: AtomicU64::new(0),
                forwarded: AtomicU64::new(0),
                fail_issuance: AtomicBool::new(false),
                issuance_gate: Mutex::new(None),
                principal: Mutex::new("identity:owner-1".into()),
                scope: Mutex::new("user:identity:owner-1".into()),
                rejection: Mutex::new(None),
                response_gate: Mutex::new(None),
                accounts_listed: AtomicU64::new(0),
            })
        }
    }

    async fn set_fixture_expiry(fixture: &AuthorityFixture, relay: &InferenceRelay, expires_at: DateTime<Utc>) {
        *lock(&fixture.expires_at) = expires_at;
        let mut state = relay.inner.authority.lock().await;
        let lease = Arc::get_mut(state.lease.as_mut().unwrap()).unwrap();
        lease.expires_at = expires_at;
        lease.authority.expires_at = expires_at.to_rfc3339();
    }

    #[derive(Debug)]
    pub(crate) struct CapturedRequest {
        pub path: String,
        pub authorization: String,
        pub api_key: Option<String>,
        pub conversation_id: String,
        pub request_id: String,
        pub account_id: Option<String>,
        pub scope: Option<String>,
        pub body: Value,
    }

    pub(crate) async fn test_control_plane(captured: mpsc::UnboundedSender<CapturedRequest>) -> String {
        test_control_plane_with_authority(captured, AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5))).await
    }

    /// A loopback Agent Auth: authority issuance, the two inference
    /// endpoints, and a fixed account list.
    pub(crate) async fn test_control_plane_with_authority(
        captured: mpsc::UnboundedSender<CapturedRequest>,
        fixture: Arc<AuthorityFixture>,
    ) -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                let fixture = fixture.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let captured = captured.clone();
                        let fixture = fixture.clone();
                        async move {
                            let path = request.uri().path().to_string();
                            let path_and_query = request
                                .uri()
                                .path_and_query()
                                .map(|value| value.as_str().to_string())
                                .unwrap_or_else(|| path.clone());
                            let headers = request.headers().clone();
                            let method = request.method().clone();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            // Ambient listener discovery (another test's preview
                            // scan) must neither break nor reach the fixture.
                            let known = matches!(
                                path.as_str(),
                                "/api/agent-accounts" | "/api/agent-auth/v2/authority"
                                    | "/api/agent-auth/v2/responses" | "/api/agent-auth/v2/messages"
                            );
                            if !known && (method == Method::HEAD || !headers.contains_key(AUTHORIZATION)) {
                                return Ok::<_, Infallible>(Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(Full::new(Bytes::new()))
                                    .unwrap());
                            }
                            assert!(
                                headers[AUTHORIZATION].to_str().unwrap().starts_with("Bearer "),
                                "every Agent Auth call is authorized"
                            );
                            let response = match path.as_str() {
                                "/api/agent-accounts" => {
                                    fixture.accounts_listed.fetch_add(1, Ordering::SeqCst);
                                    assert!(headers[AUTHORIZATION].to_str().unwrap().starts_with("Bearer comet-access-token"));
                                    json!({"accounts": [
                                        {"id":"acct-claude-1","provider":"anthropic","providerAccountId":"org-1","email":"one@example.com","status":"connected"},
                                        {"id":"acct-claude-2","provider":"anthropic","providerAccountId":"org-2","displayName":"Team Two","status":"active"},
                                        {"id":"acct-claude-off","provider":"anthropic","providerAccountId":"org-3","email":"off@example.com","status":"revoked"},
                                        {"id":"acct-openai-1","provider":"openai","providerAccountId":"user::ws-1","email":"codex@example.com","status":"connected"},
                                        {"id":"acct-openai-copy","provider":"openai","providerAccountId":"other::ws-1","email":"Codex@example.com","status":"connected"}
                                    ]})
                                }
                                "/api/agent-auth/v2/authority" => {
                                    assert!(headers[AUTHORIZATION].to_str().unwrap().starts_with("Bearer comet-access-token"));
                                    let authority_id = fixture.issued.fetch_add(1, Ordering::SeqCst) + 1;
                                    let gate = lock(&fixture.issuance_gate).clone();
                                    if let Some(gate) = gate {
                                        gate.acquire().await.unwrap().forget();
                                    }
                                    if fixture.fail_issuance.load(Ordering::SeqCst) {
                                        return Ok::<_, Infallible>(Response::builder()
                                            .status(StatusCode::SERVICE_UNAVAILABLE)
                                            .header("retry-after", "1")
                                            .body(Full::new(Bytes::from_static(br#"{"error":"temporarily_unavailable"}"#)))
                                            .unwrap());
                                    }
                                    json!({
                                        "contractVersion": 2,
                                        "token": format!("remote-agent-auth-authority-{authority_id}"),
                                        "tokenType": "Bearer",
                                        "authorityId": format!("authority-{authority_id}"),
                                        "principalId": lock(&fixture.principal).clone(),
                                        "authorityScope": lock(&fixture.scope).clone(),
                                        "expiresAt": lock(&fixture.expires_at).to_rfc3339(),
                                    })
                                }
                                "/api/agent-auth/v2/responses" | "/api/agent-auth/v2/messages" => {
                                    fixture.forwarded.fetch_add(1, Ordering::SeqCst);
                                    let header = |name: &str| {
                                        headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_string)
                                    };
                                    captured
                                        .send(CapturedRequest {
                                            path: path_and_query,
                                            authorization: header("authorization").unwrap(),
                                            api_key: header("x-api-key"),
                                            conversation_id: header("x-agent-auth-conversation-id").unwrap(),
                                            request_id: header("x-agent-auth-request-id").unwrap(),
                                            account_id: header("x-agent-auth-account-id"),
                                            scope: header("x-agent-auth-authority-scope"),
                                            body: serde_json::from_slice(&bytes).unwrap(),
                                        })
                                        .unwrap();
                                    let rejection = lock(&fixture.rejection).clone();
                                    let gate = lock(&fixture.response_gate).clone();
                                    if let Some(gate) = gate {
                                        gate.acquire().await.unwrap().forget();
                                    }
                                    if let Some((body, selected_account)) = rejection {
                                        let mut response = Response::builder()
                                            .status(StatusCode::UNAUTHORIZED)
                                            .header("x-fixture-error", "preserved");
                                        if selected_account {
                                            response = response.header("x-agent-auth-selected-account-id", "account-actual");
                                        }
                                        return Ok::<_, Infallible>(response.body(Full::new(Bytes::from(body))).unwrap());
                                    }
                                    json!({ "ok": true })
                                }
                                other => panic!("unexpected control-plane path {other}"),
                            };
                            let mut builder = Response::builder();
                            if path.starts_with("/api/agent-auth/v2/") && path != "/api/agent-auth/v2/authority" {
                                builder = builder.header("x-agent-auth-selected-account-id", "account-actual");
                            }
                            Ok::<_, Infallible>(builder.body(Full::new(Bytes::from(response.to_string()))).unwrap())
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        origin
    }

    async fn counting_control_plane(captured: mpsc::UnboundedSender<u64>) -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let captured = captured.clone();
                        async move {
                            let path = request.uri().path().to_string();
                            let response = match path.as_str() {
                                "/api/agent-auth/v2/authority" => {
                                    let _ = request.into_body().collect().await.unwrap();
                                    json!({
                                        "contractVersion": 2,
                                        "token": "remote-large-body-authority",
                                        "tokenType": "Bearer",
                                        "authorityId": "large-body-authority",
                                        "principalId": "identity:owner-1",
                                        "authorityScope": "user:identity:owner-1",
                                        "expiresAt": (Utc::now() + TimeDelta::minutes(5)).to_rfc3339(),
                                    })
                                }
                                "/api/agent-auth/v2/responses" => {
                                    let mut body = request.into_body().into_data_stream();
                                    let mut observed = 0_u64;
                                    while let Some(chunk) = body.next().await {
                                        observed += chunk.unwrap().len() as u64;
                                    }
                                    captured.send(observed).unwrap();
                                    json!({ "ok": true })
                                }
                                other => panic!("unexpected counting control-plane path {other}"),
                            };
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(response.to_string()))))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        origin
    }

    type StreamingFixture = (
        String,
        mpsc::UnboundedReceiver<Result<(), hyper::Error>>,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    );

    async fn streaming_control_plane(captured: mpsc::UnboundedSender<CapturedRequest>) -> StreamingFixture {
        streaming_control_plane_with_status(captured, StatusCode::OK, None).await
    }

    async fn streaming_control_plane_with_status(
        captured: mpsc::UnboundedSender<CapturedRequest>,
        status: StatusCode,
        tail_gate: Option<Arc<Semaphore>>,
    ) -> StreamingFixture {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (connections, streaming_connections) = mpsc::unbounded_channel::<Result<(), hyper::Error>>();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::<(bool, Result<(), hyper::Error>)>::new();
            loop {
                let stream = tokio::select! {
                    biased;
                    result = tasks.join_next(), if !tasks.is_empty() => {
                        let (streaming, result) = result.unwrap().expect("fixture connection task must not panic");
                        if streaming { connections.send(result).unwrap(); } else { result.unwrap(); }
                        continue;
                    }
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => accepted.unwrap().0,
                };
                let captured = captured.clone();
                let tail_gate = tail_gate.clone();
                tasks.spawn(async move {
                    let streaming = Arc::new(AtomicBool::new(false));
                    let serving_stream = streaming.clone();
                    let service = service_fn(move |request: Request<Incoming>| {
                        let captured = captured.clone();
                        let streaming = serving_stream.clone();
                        let tail_gate = tail_gate.clone();
                        async move {
                            let method = request.method().clone();
                            let root_probe = request.uri().path_and_query().is_some_and(|target| target.as_str() == "/");
                            let path = request.uri().path().to_string();
                            let headers = request.headers().clone();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            // Ambient listener discovery must not consume or break the fixture.
                            if method == Method::HEAD && root_probe && bytes.is_empty()
                                && !headers.contains_key(AUTHORIZATION) && !headers.contains_key("x-api-key") {
                                return Ok::<_, Infallible>(Response::builder().status(StatusCode::NOT_FOUND)
                                    .header(CONNECTION, "close")
                                    .body(BodyExt::boxed(Full::new(Bytes::new()))).unwrap());
                            }
                            assert_eq!(method, Method::POST, "unexpected fixture request: {method} {path}");
                            if path == "/api/agent-auth/v2/responses" {
                                streaming.store(true, Ordering::Release);
                                captured
                                    .send(CapturedRequest {
                                        path: path.clone(),
                                        authorization: headers[AUTHORIZATION].to_str().unwrap().to_string(),
                                        api_key: None,
                                        conversation_id: headers["x-agent-auth-conversation-id"].to_str().unwrap().to_string(),
                                        request_id: headers["x-agent-auth-request-id"].to_str().unwrap().to_string(),
                                        account_id: None,
                                        scope: None,
                                        body: serde_json::from_slice(&bytes).unwrap(),
                                    })
                                    .unwrap();
                                let body = futures::stream::once(async {
                                    Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"data: first\n\n")))
                                })
                                .chain(futures::stream::once(async move {
                                    match tail_gate {
                                        Some(gate) => {
                                            gate.acquire().await.unwrap().forget();
                                        }
                                        None => futures::future::pending::<()>().await,
                                    }
                                    Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"data: tail\n\n")))
                                }));
                                return Ok::<_, Infallible>(Response::builder()
                                    .status(status)
                                    .header("x-fixture-error", "preserved")
                                    .body(BodyExt::boxed(StreamBody::new(body)))
                                    .unwrap());
                            }
                            assert_eq!(path, "/api/agent-auth/v2/authority");
                            let authority = json!({
                                "contractVersion": 2,
                                "token": "remote-agent-auth-authority",
                                "tokenType": "Bearer",
                                "authorityId": "authority-1",
                                "principalId": "identity:owner-1",
                                "authorityScope": "user:identity:owner-1",
                                "expiresAt": (Utc::now() + TimeDelta::minutes(5)).to_rfc3339(),
                            });
                            let body = futures::stream::once(async move {
                                Ok::<_, Infallible>(Frame::data(Bytes::from(authority.to_string())))
                            });
                            Ok(Response::new(BodyExt::boxed(StreamBody::new(body))))
                        }
                    });
                    let result = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                    (streaming.load(Ordering::Acquire), result)
                });
            }
            tasks.abort_all();
            while let Some(result) = tasks.join_next().await {
                match result {
                    Ok((_, result)) => result.unwrap(),
                    Err(error) => assert!(error.is_cancelled(), "fixture connection task panicked: {error}"),
                }
            }
        });
        (origin, streaming_connections, shutdown_tx, server)
    }

    #[tokio::test]
    async fn distinct_routes_share_preparation_renewal_and_waiter_cancellation() {
        let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::seconds(20));
        let (tx, _rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(tx, fixture.clone()).await;
        let relay = start_relay(&origin, static_token());
        let sessions = (0..16).map(|index| format!("shared-session-{index}")).collect::<Vec<_>>();
        let prepared = futures::future::join_all(
            sessions.iter().map(|session| relay.prepare(session, HarnessId::Codex, Some(CODEX_MODEL), None)),
        )
        .await;
        let routes = prepared
            .into_iter()
            .map(|prepared| route_of(&relay, &prepared.unwrap().token))
            .collect::<Vec<_>>();
        relay
            .prepare("other-provider", HarnessId::ClaudeCode, Some("claude-opus-5"), Some("pinned-account"))
            .await
            .unwrap();
        for route in &routes {
            assert_eq!(relay.current_authority(route).await.unwrap().authority.authority_id, "authority-1");
        }
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 1);
        let boundary = Utc::now() + TimeDelta::milliseconds(900);
        set_fixture_expiry(&fixture, &relay, boundary).await;
        let calls = futures::future::join_all(routes.iter().map(|route| relay.current_authority(route)));
        tokio::pin!(calls);
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut calls).await.is_err());
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 1);
        routes[0].cancellation.cancel();
        *lock(&fixture.expires_at) = Utc::now() + TimeDelta::minutes(5);
        let renewed = tokio::time::timeout(Duration::from_secs(2), &mut calls).await.unwrap();
        assert_eq!(renewed[0].as_ref().err().unwrap().reason, "route_cancelled");
        for lease in &renewed[1..] {
            assert_eq!(lease.as_ref().unwrap().authority.authority_id, "authority-2");
        }
        assert!(Utc::now() >= boundary);
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn credential_generation_discards_cached_authority_and_signout_fails_closed() {
        let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5));
        let token = Arc::new(ProfileToken::default());
        let (tx, _rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(tx, fixture.clone()).await;
        let relay = start_relay(&origin, token.clone());
        let prepared = relay.prepare("generation-session", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
        let route = route_of(&relay, &prepared.token);
        let first = relay.current_authority(&route).await.unwrap().credential_generation;
        token.generation.fetch_add(1, Ordering::SeqCst);
        let renewed = relay.current_authority(&route).await.unwrap();
        assert_ne!(renewed.credential_generation, first);
        assert_eq!(renewed.authority.authority_id, "authority-2");
        token.signed_out.store(true, Ordering::SeqCst);
        assert!(relay.current_authority(&route).await.is_err());
        match relay.prepare("signed-out", HarnessId::Codex, Some(CODEX_MODEL), None).await {
            Err(PrepareError::Authority(error)) => assert!(error.signed_out, "{error:?}"),
            other => panic!("a signed-out profile prepares nothing: {other:?}"),
        }
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn credential_change_during_issuance_cannot_publish_old_profile_authority() {
        for signed_out in [false, true] {
            let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5));
            let gate = Arc::new(Semaphore::new(0));
            *lock(&fixture.issuance_gate) = Some(gate.clone());
            let token = Arc::new(ProfileToken::default());
            let (tx, _rx) = mpsc::unbounded_channel();
            let origin = test_control_plane_with_authority(tx, fixture.clone()).await;
            let relay = start_relay(&origin, token.clone());
            let preparing = {
                let relay = relay.clone();
                tokio::spawn(async move { relay.prepare("changing-profile", HarnessId::Codex, Some(CODEX_MODEL), None).await })
            };
            tokio::time::timeout(Duration::from_secs(2), async {
                while fixture.issued.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            token.generation.fetch_add(1, Ordering::SeqCst);
            token.signed_out.store(signed_out, Ordering::SeqCst);
            gate.add_permits(1);
            assert!(preparing.await.unwrap().is_err());
            assert!(relay.inner.authority.lock().await.lease.is_none());
            assert!(lock(&relay.inner.route_state).active.is_empty());
            if !signed_out {
                *lock(&fixture.issuance_gate) = None;
                // The new credential owes nothing to the old one's cooldown.
                relay.prepare("new-profile", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
                assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
            }
        }
    }

    #[tokio::test]
    async fn profile_change_cannot_rebind_old_routes_or_retired_credentials() {
        for change_owner in [false, true] {
            let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5));
            let token = Arc::new(ProfileToken::default());
            let (tx, _rx) = mpsc::unbounded_channel();
            let origin = test_control_plane_with_authority(tx, fixture.clone()).await;
            let relay = start_relay(&origin, token.clone());
            let prepared = relay.prepare("profile-route", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
            let old = route_of(&relay, &prepared.token);
            let retired = relay.prepare("retired-profile", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
            relay.remove(&retired.token);
            token.generation.fetch_add(1, Ordering::SeqCst);
            *lock(&fixture.scope) = "user:another-scope".into();
            if change_owner {
                *lock(&fixture.principal) = "identity:another-owner".into();
            }
            let replacement = relay.prepare("retired-profile", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
            assert_ne!(replacement.token, retired.token);
            assert!(relay.current_authority(&old).await.is_err());
            assert!(old.cancellation.is_cancelled());
            let response = reqwest::Client::new()
                .get(format!("{}/v1/models", retired.base_url))
                .bearer_auth(&retired.token)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::GONE);
            assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn signed_out_profile_fences_cached_authority_and_both_wait_paths() {
        for wait in ["cached", "mutex", "expiry"] {
            let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::seconds(20));
            let token = Arc::new(ProfileToken::default());
            let (tx, _rx) = mpsc::unbounded_channel();
            let origin = test_control_plane_with_authority(tx, fixture.clone()).await;
            let relay = start_relay(&origin, token.clone());
            let prepared = relay.prepare("signout-wait", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
            let route = route_of(&relay, &prepared.token);
            if wait == "expiry" {
                set_fixture_expiry(&fixture, &relay, Utc::now() + TimeDelta::milliseconds(900)).await;
            }
            let held = if wait == "mutex" { Some(relay.inner.authority.lock().await) } else { None };
            let call = relay.current_authority(&route);
            tokio::pin!(call);
            if wait != "cached" {
                assert!(tokio::time::timeout(Duration::from_millis(50), &mut call).await.is_err());
            }
            token.signed_out.store(true, Ordering::SeqCst);
            drop(held);
            assert!(tokio::time::timeout(Duration::from_secs(2), &mut call).await.unwrap().is_err(), "{wait}");
            assert_eq!(fixture.issued.load(Ordering::SeqCst), 1, "{wait}");
            assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 0, "{wait}");
        }
    }

    #[tokio::test]
    async fn transient_issuance_cools_down_routes_without_forwarding() {
        let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::seconds(20));
        let (tx, _rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(tx, fixture.clone()).await;
        let relay = start_relay(&origin, static_token());
        let first = relay.prepare("cooldown", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
        let sibling = relay.prepare("cooldown-sibling", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
        set_fixture_expiry(&fixture, &relay, Utc::now() - TimeDelta::seconds(1)).await;
        fixture.fail_issuance.store(true, Ordering::SeqCst);
        let http = reqwest::Client::new();
        let url = format!("{}/v1/responses", first.base_url);
        let requests = (0..16).map(|index| {
            http.post(&url)
                .bearer_auth(if index % 2 == 0 { &first.token } else { &sibling.token })
                .json(&json!({ "input": "cooldown" }))
                .send()
        });
        let responses = tokio::time::timeout(Duration::from_secs(2), futures::future::join_all(requests))
            .await
            .unwrap();
        for response in responses {
            let response = response.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers()["retry-after"], "1");
            assert_eq!(response.json::<Value>().await.unwrap()["error"], "agent_auth_unavailable");
        }
        assert!(relay.prepare("cooldown-prepare", HarnessId::Codex, Some(CODEX_MODEL), None).await.is_err());
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 0);
        let (started, delay) = relay.inner.authority.lock().await.retry_at.unwrap();
        tokio::time::sleep(delay.saturating_sub(started.elapsed()) + Duration::from_millis(20)).await;
        fixture.fail_issuance.store(false, Ordering::SeqCst);
        *lock(&fixture.expires_at) = Utc::now() + TimeDelta::minutes(5);
        relay.prepare("recovered", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn authority_rejection_never_replays_and_stale_rejection_cannot_evict_new_grant() {
        let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5));
        let body = r#"{"error":"agent_auth_v2_authority_invalid"}"#;
        *lock(&fixture.rejection) = Some((body.into(), false));
        let gate = Arc::new(Semaphore::new(0));
        *lock(&fixture.response_gate) = Some(gate.clone());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(tx, fixture.clone()).await;
        let relay = start_relay(&origin, static_token());
        let route = relay.prepare("rejected", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
        let http = reqwest::Client::new();
        let mut requests = tokio::task::JoinSet::new();
        for input in ["first", "stale"] {
            let request = http
                .post(format!("{}/v1/responses", route.base_url))
                .bearer_auth(&route.token)
                .json(&json!({ "input": input }));
            requests.spawn(async move { request.send().await.unwrap() });
        }
        for _ in 0..2 {
            assert_eq!(rx.recv().await.unwrap().authorization, "Bearer remote-agent-auth-authority-1");
        }
        gate.add_permits(1);
        let rejected = requests.join_next().await.unwrap().unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(rejected.headers()["x-fixture-error"], "preserved");
        assert_eq!(rejected.text().await.unwrap(), body);
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 2);
        relay.prepare("independent", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
        gate.add_permits(1);
        assert_eq!(requests.join_next().await.unwrap().unwrap().text().await.unwrap(), body);
        relay.prepare("after-stale", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 2);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn provider_and_unrecognized_401_responses_never_invalidate_authority() {
        for (body, selected_account) in [
            (r#"{"error":"agent_auth_v2_authority_invalid"}"#.to_string(), true),
            (r#"{"error":"provider_credentials_invalid"}"#.to_string(), false),
            ("not-json".to_string(), false),
            (format!(r#"{{"error":"agent_auth_v2_authority_invalid","padding":"{}"}}"#, "x".repeat(4096)), false),
        ] {
            let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5));
            *lock(&fixture.rejection) = Some((body.clone(), selected_account));
            let (tx, _rx) = mpsc::unbounded_channel();
            let origin = test_control_plane_with_authority(tx, fixture.clone()).await;
            let relay = start_relay(&origin, static_token());
            let route = relay.prepare("provider-error", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
            let response = reqwest::Client::new()
                .post(format!("{}/v1/responses", route.base_url))
                .bearer_auth(&route.token)
                .json(&json!({ "input": "once" }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(response.headers()["x-fixture-error"], "preserved");
            assert_eq!(response.text().await.unwrap(), body);
            relay.prepare("after-provider-error", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
            assert_eq!(fixture.issued.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn stalled_401_preserves_prefix_tail_and_authority() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tail = Arc::new(Semaphore::new(0));
        let (origin, mut connections, shutdown, server) =
            streaming_control_plane_with_status(tx, StatusCode::UNAUTHORIZED, Some(tail.clone())).await;
        let relay = start_relay(&origin, static_token());
        let prepared = relay.prepare("stalled", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
        let lease = relay.inner.authority.lock().await.lease.clone().unwrap();
        let response = tokio::time::timeout(
            Duration::from_secs(3),
            reqwest::Client::new()
                .post(format!("{}/v1/responses", prepared.base_url))
                .bearer_auth(&prepared.token)
                .json(&json!({ "input": "once" }))
                .send(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["x-fixture-error"], "preserved");
        assert_eq!(rx.recv().await.unwrap().body["input"], "once");
        let mut body = response.bytes_stream();
        assert_eq!(body.next().await.unwrap().unwrap(), Bytes::from_static(b"data: first\n\n"));
        tail.add_permits(1);
        assert_eq!(body.next().await.unwrap().unwrap(), Bytes::from_static(b"data: tail\n\n"));
        assert!(body.next().await.is_none());
        let current = relay.inner.authority.lock().await.lease.clone().unwrap();
        assert!(Arc::ptr_eq(&lease, &current));
        assert!(rx.try_recv().is_err());
        relay.remove(&prepared.token);
        shutdown.send(()).unwrap();
        server.await.unwrap();
        while let Ok(result) = connections.try_recv() {
            result.unwrap();
        }
    }

    #[tokio::test]
    async fn renewal_cannot_rebind_an_existing_route_to_another_owner() {
        let (captured_tx, _) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let relay = start_relay(&origin, static_token());
        let prepared = relay.prepare("session-owner", HarnessId::Codex, Some("gpt-5.6-sol"), None).await.unwrap();
        let route = route_of(&relay, &prepared.token);
        let changed_owner = Route {
            request: route.request.clone(),
            token: route.token.clone(),
            local_session_id: None,
            owner_subject: "identity:someone-else".into(),
            authority_scope: route.authority_scope.clone(),
            cancellation: CancellationToken::new(),
            serial: route.serial,
        };
        assert!(relay.current_authority(&changed_owner).await.is_err());
        assert!(changed_owner.cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn issues_authority_streams_through_loopback_and_removes_locally() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let relay = start_relay(&origin, static_token());
        let session = "7b0e6a52-7f7e-4a39-9a2c-1b8c0f6f3d11";
        let route = relay.prepare(session, HarnessId::Codex, Some("gpt-5.6-sol"), None).await.unwrap();
        assert_eq!(route.provider, Provider::Openai);
        assert_eq!(route.token.len(), 64);
        assert!(route.base_url.starts_with("http://127.0.0.1:"));

        let http = reqwest::Client::new();
        let models = http
            .get(format!("{}/v1/models", route.base_url))
            .bearer_auth(&route.token)
            .send()
            .await
            .unwrap();
        assert_eq!(models.status(), reqwest::StatusCode::OK);
        let catalog: Value = models.json().await.unwrap();
        assert_eq!(catalog["data"][0]["id"], "openai-codex/gpt-5.6-sol");

        let denied = http
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth("wrong-local-token")
            .json(&json!({ "model": "gpt-5.6-sol" }))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);

        // The wrong provider's endpoint, and a query it didn't ask for.
        let mismatch = http
            .post(format!("{}/v1/messages", route.base_url))
            .bearer_auth(&route.token)
            .json(&json!({ "model": "gpt-5.6-sol" }))
            .send()
            .await
            .unwrap();
        assert_eq!(mismatch.status(), reqwest::StatusCode::CONFLICT);
        let query = http
            .post(format!("{}/v1/responses?key=secret", route.base_url))
            .bearer_auth(&route.token)
            .json(&json!({ "model": "gpt-5.6-sol" }))
            .send()
            .await
            .unwrap();
        assert_eq!(query.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(captured_rx.try_recv().is_err());

        let response = http
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .header("x-request-id", "request-1")
            .header("x-agent-auth-account-id", "forged-account")
            .json(&json!({ "model": "gpt-5.6-sol", "input": "hello" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap(), json!({ "ok": true }));
        let captured = captured_rx.recv().await.unwrap();
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-authority-1");
        assert_eq!(captured.conversation_id, session);
        assert_eq!(captured.request_id, "request-1");
        assert_eq!(captured.account_id, None, "a child never picks the account");
        assert_eq!(captured.scope.as_deref(), Some("user:identity:owner-1"));
        assert_eq!(captured.body["input"], "hello");

        relay.remove(&route.token);
        let removed = http
            .get(format!("{}/v1/models", route.base_url))
            .bearer_auth(&route.token)
            .send()
            .await
            .unwrap();
        assert_eq!(removed.status(), reqwest::StatusCode::GONE);
        assert_eq!(
            removed.json::<Value>().await.unwrap(),
            json!({ "error": "inference_route_expired", "restart_required": true })
        );
    }

    #[tokio::test]
    async fn streams_requests_larger_than_the_former_relay_ceiling() {
        const CHUNK_BYTES: usize = 256 * 1024;
        const CHUNK_COUNT: usize = 132;
        const BODY_BYTES: usize = CHUNK_BYTES * CHUNK_COUNT;

        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = counting_control_plane(captured_tx).await;
        let relay = start_relay(&origin, static_token());
        let route = relay.prepare("session-large-body", HarnessId::Codex, Some("gpt-5.6-sol"), None).await.unwrap();
        let chunk = Bytes::from(vec![b'x'; CHUNK_BYTES]);
        let body = reqwest::Body::wrap_stream(stream::iter(
            (0..CHUNK_COUNT).map(move |_| Ok::<_, io::Error>(chunk.clone())),
        ));
        let response = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .header(CONTENT_LENGTH, BODY_BYTES)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(captured_rx.recv().await.unwrap(), BODY_BYTES as u64);
    }

    #[test]
    fn projects_chat_ids_that_are_not_uuids_to_stable_agent_auth_uuids() {
        let local_id = "local-chat-b5b85d0f52a29e39da7656ab";
        let projected = agent_auth_logical_session_id(local_id).into_owned();
        let parsed = uuid::Uuid::parse_str(&projected).unwrap();
        assert_eq!(projected, agent_auth_logical_session_id(local_id));
        assert_eq!(parsed.as_bytes()[6] >> 4, 8);
        assert_eq!(parsed.as_bytes()[8] & 0xc0, 0x80);
        // Crew's projection of the same id, so both agree on its session.
        let mut hasher = Sha256::new();
        hasher.update(b"comet-agent-auth-session\0");
        hasher.update(local_id.as_bytes());
        assert_eq!(&parsed.as_bytes()[..6], &hasher.finalize()[..6]);
        let chat = "7b0e6a52-7f7e-4a39-9a2c-1b8c0f6f3d11";
        assert!(matches!(agent_auth_logical_session_id(chat), Cow::Borrowed(id) if id == chat));
        // Only the canonical spelling passes as is.
        assert!(matches!(agent_auth_logical_session_id(&chat.to_uppercase()), Cow::Owned(_)));
        assert!(matches!(agent_auth_logical_session_id("session-1"), Cow::Owned(_)));
    }

    #[tokio::test]
    async fn projects_a_local_chat_id_and_retires_its_local_token() {
        let local_id = "local-chat-b5b85d0f52a29e39da7656ab";
        let projected = agent_auth_logical_session_id(local_id).into_owned();
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let relay = start_relay(&origin, static_token());
        let route = relay.prepare(local_id, HarnessId::Codex, Some("gpt-5.6-sol"), None).await.unwrap();
        {
            let active = route_of(&relay, &route.token);
            assert_eq!(active.request.logical_session_id, projected);
            assert_eq!(active.session_id(), local_id);
        }
        let http = reqwest::Client::new();
        let response = http
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .json(&json!({ "input": "hello" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(captured_rx.recv().await.unwrap().conversation_id, projected);
        relay.remove(&route.token);
        let removed = http
            .get(format!("{}/v1/models", route.base_url))
            .bearer_auth(&route.token)
            .send()
            .await
            .unwrap();
        assert_eq!(removed.status(), StatusCode::GONE);
    }

    /// Upstream prepares a chat's route on every spawn (a warm start, the run,
    /// each replacement): the same chat and account get the same token, so a
    /// parked or warm runtime's environment still matches; a model change
    /// keeps it too.
    #[tokio::test]
    async fn the_same_chat_and_account_keep_one_route_and_token() {
        let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let relay = start_relay(&origin, static_token());
        let first = relay.prepare("chat", HarnessId::ClaudeCode, Some("claude-opus-5-5"), None).await.unwrap();
        let again = relay.prepare("chat", HarnessId::ClaudeCode, Some("claude-opus-5-5"), None).await.unwrap();
        assert_eq!((again.token.as_str(), again.serial), (first.token.as_str(), first.serial));
        let other_model = relay.prepare("chat", HarnessId::ClaudeCode, Some("claude-sonnet-5-5"), None).await.unwrap();
        assert_eq!((other_model.token.as_str(), other_model.serial), (first.token.as_str(), first.serial));
        assert_eq!(other_model.model, "claude-sonnet-5-5");
        assert_eq!(relay.current_serial("chat", HarnessId::ClaudeCode, None), Some(first.serial));
        assert_eq!(relay.current_serial("chat", HarnessId::ClaudeCode, Some("acct")), None);
        let catalog: Value = reqwest::Client::new()
            .get(format!("{}/v1/models", first.base_url))
            .bearer_auth(&first.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(catalog["data"][0]["id"], "anthropic/claude-sonnet-5-5");
        // Another chat is another route.
        let other_chat = relay.prepare("other-chat", HarnessId::ClaudeCode, Some(CLAUDE_MODEL), None).await.unwrap();
        assert_ne!(other_chat.token, first.token);
        assert_eq!(other_chat.model, "claude-opus-5-5");
    }

    /// Choosing another account (or provider) retires the chat's route: its
    /// token then follows the chat's current route for the same provider,
    /// as a worker left behind by the replaced runtime would use it.
    #[tokio::test]
    async fn another_account_retires_the_chats_route_and_its_token_follows_the_replacement() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let relay = start_relay(&origin, static_token());
        let automatic = relay.prepare("chat", HarnessId::ClaudeCode, Some(CLAUDE_MODEL), None).await.unwrap();
        let pinned = relay.prepare("chat", HarnessId::ClaudeCode, Some(CLAUDE_MODEL), Some("acct-claude-2")).await.unwrap();
        assert_ne!(pinned.token, automatic.token);
        assert_ne!(pinned.serial, automatic.serial);
        assert!(route_of(&relay, &pinned.token).request.requested_account_id.as_deref() == Some("acct-claude-2"));
        assert!(lock(&relay.inner.route_state).retired.contains_key(&automatic.token));
        assert_eq!(relay.current_serial("chat", HarnessId::ClaudeCode, None), None);

        let response = reqwest::Client::new()
            .post(format!("{}/v1/messages?beta=true", automatic.base_url))
            .header("x-api-key", &automatic.token)
            .json(&json!({ "model": "claude-opus-5-5", "messages": [] }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let captured = captured_rx.recv().await.unwrap();
        assert_eq!(captured.path, "/api/agent-auth/v2/messages?beta=true");
        assert_eq!(captured.account_id.as_deref(), Some("acct-claude-2"));
        assert_eq!(captured.api_key, None, "the loopback token never leaves");

        // Switching provider: Codex for the same chat retires Claude's route,
        // and the old token has nothing of its provider to follow.
        let codex = relay.prepare("chat", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
        let gone = reqwest::Client::new()
            .get(format!("{}/v1/models", codex.base_url))
            .bearer_auth(&pinned.token)
            .send()
            .await
            .unwrap();
        assert_eq!(gone.status(), StatusCode::GONE);
        assert_eq!(
            gone.json::<Value>().await.unwrap(),
            json!({ "error": "inference_route_expired", "restart_required": false })
        );
    }

    #[tokio::test]
    async fn route_removal_ends_an_active_inference_stream() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let (origin, mut streaming_connections, shutdown, server) = streaming_control_plane(captured_tx).await;
        let relay = start_relay(&origin, static_token());
        let route = relay.prepare("session-1", HarnessId::Codex, Some("gpt-5.6-sol"), None).await.unwrap();
        let response = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .header("x-request-id", "request-cancel")
            .json(&json!({ "model": "gpt-5.6-sol", "input": "hello" }))
            .send()
            .await
            .unwrap();
        captured_rx.recv().await.unwrap();

        relay.remove(&route.token);
        assert_eq!(response.bytes().await.unwrap(), Bytes::from_static(b"data: first\n\n"));
        let result = tokio::time::timeout(Duration::from_secs(5), streaming_connections.recv())
            .await
            .expect("route removal must close the upstream stream")
            .expect("streaming fixture server must not terminate early");
        if let Err(error) = result {
            assert!(error.is_incomplete_message(), "unexpected upstream connection error: {error}");
        }
        shutdown.send(()).unwrap();
        server.await.expect("fixture server must not panic");
    }

    /// A route is for a model its harness's provider serves, named: a run
    /// that names none was given its harness's own default before
    /// (`CrewInference::prepare`), and another provider's is never offered.
    #[test]
    fn a_route_is_for_a_named_model_of_its_harness_s_provider() {
        assert!(routable(HarnessId::ClaudeCode, "claude-opus-5-5"));
        assert!(routable(HarnessId::ClaudeCode, "anthropic/claude-fable-5-2"));
        assert!(!routable(HarnessId::ClaudeCode, "gateway/sol"));
        assert!(routable(HarnessId::Codex, "gpt-6.2-sol"));
        assert!(routable(HarnessId::Codex, "openai/gpt-6.2-astra"));
        assert!(!routable(HarnessId::Codex, "anthropic/claude-opus-5-5"));
        assert!(!routable(HarnessId::Pi, "gpt-6.2-sol"));
        for unnamed in [None, Some(""), Some("default")] {
            assert_eq!(inference_binding(HarnessId::Codex, unnamed), None, "{unnamed:?}");
        }
    }

    #[tokio::test]
    async fn shutdown_retires_every_route_and_stops_serving() {
        let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let relay = start_relay(&origin, static_token());
        let route = relay.prepare("chat", HarnessId::Codex, Some(CODEX_MODEL), None).await.unwrap();
        let live = route_of(&relay, &route.token);
        relay.shutdown();
        assert!(live.cancellation.is_cancelled());
        assert!(lock(&relay.inner.route_state).active.is_empty());
        assert_eq!(relay.current_serial("chat", HarnessId::Codex, None), None);
        // New connections aren't taken any more.
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            reqwest::Client::new().get(format!("{}/v1/models", route.base_url)).bearer_auth(&route.token).send(),
        )
        .await;
        assert!(!matches!(refused, Ok(Ok(response)) if response.status().is_success()));
    }

    #[tokio::test]
    async fn authenticates_anthropic_sdk_requests_without_forwarding_the_loopback_token() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let relay = start_relay(&origin, static_token());
        let route = relay
            .prepare("session-anthropic", HarnessId::ClaudeCode, Some("claude-opus-5"), None)
            .await
            .unwrap();
        assert!(route.token.starts_with("sk-ant-oat01-"));
        let response = reqwest::Client::new()
            .post(format!("{}/v1/messages?beta=true", route.base_url))
            .header("x-api-key", &route.token)
            .json(&json!({ "model": "claude-opus-5", "messages": [] }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap(), json!({ "ok": true }));
        let captured = captured_rx.recv().await.unwrap();
        assert_eq!(captured.path, "/api/agent-auth/v2/messages?beta=true");
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-authority-1");
        assert_eq!(captured.api_key, None);
        assert_eq!(captured.conversation_id, agent_auth_logical_session_id("session-anthropic"));
        assert_eq!(captured.body["model"], "claude-opus-5");
    }

    #[test]
    fn accepts_scoped_and_legacy_unscoped_v2_principal_authorities() {
        let authority = AgentInferenceAuthority {
            contract_version: 2,
            token: "v2-authority-token".into(),
            token_type: "Bearer".into(),
            authority_id: "authority-1".into(),
            principal_id: "identity:owner-1".into(),
            authority_scope: Some("user:identity:owner-1".into()),
            expires_at: "2099-01-01T00:00:00Z".into(),
        };
        assert!(validate_authority(&authority).is_ok());
        let mut invalid = authority.clone();
        invalid.contract_version = 1;
        assert!(validate_authority(&invalid).is_err());
        let mut unscoped = authority.clone();
        unscoped.authority_scope = None;
        assert!(validate_authority(&unscoped).is_ok());
        let mut empty_scope = authority.clone();
        empty_scope.authority_scope = Some(String::new());
        assert!(validate_authority(&empty_scope).is_err());
        let mut expired = authority;
        expired.expires_at = "2001-01-01T00:00:00Z".into();
        assert!(validate_authority(&expired).is_err());
    }

    #[test]
    fn accepts_exactly_one_matching_loopback_credential() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer local-token".parse().unwrap());
        assert_eq!(relay_token(&headers).as_deref(), Some("local-token"));
        headers.remove(AUTHORIZATION);
        headers.insert("x-api-key", "local-token".parse().unwrap());
        assert_eq!(relay_token(&headers).as_deref(), Some("local-token"));
        headers.insert(AUTHORIZATION, "Bearer other-token".parse().unwrap());
        assert_eq!(relay_token(&headers), None);
    }

    #[test]
    fn local_relay_token_uses_oauth_shape_only_for_anthropic() {
        let anthropic = local_relay_token(Provider::Anthropic);
        assert!(anthropic.starts_with("sk-ant-oat01-"));
        assert!(!anthropic.chars().any(char::is_whitespace));
        assert!(!local_relay_token(Provider::Openai).starts_with("sk-ant-oat01-"));
    }

    #[test]
    fn binds_only_claude_code_to_anthropic_and_codex_to_openai() {
        assert_eq!(
            inference_binding(HarnessId::ClaudeCode, Some("claude-opus-5")),
            Some((Provider::Anthropic, "claude-opus-5".into()))
        );
        assert_eq!(
            inference_binding(HarnessId::ClaudeCode, Some("anthropic/claude-opus-5")),
            Some((Provider::Anthropic, "claude-opus-5".into()))
        );
        assert_eq!(inference_binding(HarnessId::ClaudeCode, Some("opus")), Some((Provider::Anthropic, "opus".into())));
        assert_eq!(inference_binding(HarnessId::ClaudeCode, Some("openai/gpt-5.6-sol")), None);
        assert_eq!(
            inference_binding(HarnessId::Codex, Some("openai-codex/gpt-5.6-sol")),
            Some((Provider::Openai, "gpt-5.6-sol".into()))
        );
        assert_eq!(inference_binding(HarnessId::Codex, Some("prime-inference/x-ai/grok-4.20")), None);
        for harness in [HarnessId::Pi, HarnessId::Hermes, HarnessId::Cursor, HarnessId::Antigravity, HarnessId::Muse, HarnessId::Mock] {
            assert_eq!(inference_binding(harness, Some("anthropic/claude-opus-5")), None, "{harness:?}");
        }
    }

    #[test]
    fn route_requests_preserve_automatic_and_pinned_account_selection() {
        let automatic = inference_route_request("session-1", HarnessId::Codex, Some("gpt-5.6-sol"), None).unwrap();
        assert_eq!(automatic.logical_session_id, "session-1");
        assert_eq!(automatic.provider, Provider::Openai);
        assert_eq!(automatic.model, "gpt-5.6-sol");
        assert_eq!(automatic.requested_account_id, None);
        let pinned =
            inference_route_request("session-1", HarnessId::ClaudeCode, Some("claude-opus-5"), Some("opaque-account-id"))
                .unwrap();
        assert_eq!(pinned.provider, Provider::Anthropic);
        assert_eq!(pinned.requested_account_id.as_deref(), Some("opaque-account-id"));
        assert!(matches!(
            inference_route_request("session-1", HarnessId::Codex, Some(CODEX_MODEL), Some(" ")),
            Err(PrepareError::InvalidAccount)
        ));
        assert!(matches!(
            inference_route_request("session-1", HarnessId::Hermes, Some("anthropic/claude-fable-5-1"), None),
            Err(PrepareError::Unsupported)
        ));
    }
}
