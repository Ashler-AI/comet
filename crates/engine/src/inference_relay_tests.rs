#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::AgentInferenceAuthority;
    use async_trait::async_trait;
    use futures::stream;
    use http_body_util::{BodyExt, Full, StreamBody};
    use hyper::{body::Frame, body::Incoming, service::service_fn};
    use serde_json::Value;
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tokio::sync::mpsc;

    #[derive(Clone)]
    struct LogWriter(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
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
        cancellation: zeron_harness::CancellationToken,
        request_id: &str,
    ) -> InstrumentedRelayStream {
        InstrumentedRelayStream::new(
            Box::pin(stream::iter(items)),
            cancellation,
            relay_stream_context(request_id),
            StatusCode::OK,
        )
    }
    #[tokio::test(flavor = "current_thread")]
    async fn logs_every_relay_stream_termination_with_request_context() {
        let logs = Arc::new(parking_lot::Mutex::new(Vec::new()));
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
            zeron_harness::CancellationToken::new(),
            "request-complete",
        );
        assert_eq!(
            complete.next().await.unwrap().unwrap(),
            Bytes::from_static(b"abc")
        );
        assert!(complete.next().await.is_none());

        let cancellation = zeron_harness::CancellationToken::new();
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
            zeron_harness::CancellationToken::new(),
            "request-failed",
        );
        assert_eq!(
            failed.next().await.unwrap().unwrap_err().to_string(),
            "body closed"
        );

        drop(InstrumentedRelayStream::new(
            Box::pin(stream::pending()),
            zeron_harness::CancellationToken::new(),
            relay_stream_context("request-dropped"),
            StatusCode::OK,
        ));
        let logs = String::from_utf8(logs.lock().clone()).unwrap();
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
        assert!(
            logs.lines()
                .any(|line| line.contains("request-failed") && line.contains("body closed"))
        );
    }

    struct StaticToken;

    #[async_trait]
    impl zeron_rpc::TokenSource for StaticToken {
        async fn token(&self) -> Result<zeron_proto::AccessToken, zeron_rpc::TokenError> {
            Ok(zeron_proto::AccessToken { value: "comet-access-token".into(), generation: 0 })
        }

        fn invalidate(&self, _generation: u64) {}
    }

    #[derive(Default)]
    struct ProfileToken {
        signed_out: AtomicBool,
        generation: AtomicU64,
    }

    #[async_trait]
    impl zeron_rpc::TokenSource for ProfileToken {
        async fn token(&self) -> Result<zeron_proto::AccessToken, zeron_rpc::TokenError> {
            if self.signed_out.load(Ordering::SeqCst) {
                Err(zeron_rpc::TokenError::SignedOut)
            } else {
                Ok(zeron_proto::AccessToken {
                    value: "comet-access-token".into(),
                    generation: self.generation.load(Ordering::SeqCst),
                })
            }
        }

        fn invalidate(&self, generation: u64) {
            let _ = self.generation.compare_exchange(generation, generation + 1, Ordering::SeqCst, Ordering::SeqCst);
        }
    }

    // Local response fixtures exercise relay scheduling, not cryptographic renewal.
    struct AuthorityFixture {
        expires_at: parking_lot::Mutex<DateTime<Utc>>,
        issued: AtomicU64,
        forwarded: AtomicU64,
        fail_issuance: AtomicBool,
        issuance_gate: parking_lot::Mutex<Option<Arc<Semaphore>>>,
        principal: parking_lot::Mutex<String>,
        scope: parking_lot::Mutex<String>,
        rejection: parking_lot::Mutex<Option<(String, bool)>>,
        response_gate: parking_lot::Mutex<Option<Arc<Semaphore>>>,
    }

    impl AuthorityFixture {
        fn new(expires_at: DateTime<Utc>) -> Arc<Self> {
            Arc::new(Self {
                expires_at: parking_lot::Mutex::new(expires_at),
                issued: AtomicU64::new(0),
                forwarded: AtomicU64::new(0),
                fail_issuance: AtomicBool::new(false),
                issuance_gate: parking_lot::Mutex::new(None),
                principal: parking_lot::Mutex::new("identity:owner-1".into()),
                scope: parking_lot::Mutex::new("user:identity:owner-1".into()),
                rejection: parking_lot::Mutex::new(None),
                response_gate: parking_lot::Mutex::new(None),
            })
        }
    }

    async fn set_fixture_expiry(
        fixture: &AuthorityFixture,
        relay: &InferenceRelay,
        expires_at: DateTime<Utc>,
    ) {
        *fixture.expires_at.lock() = expires_at;
        let mut state = relay.inner.authority.lock().await;
        let lease = Arc::get_mut(state.lease.as_mut().unwrap()).unwrap();
        lease.expires_at = expires_at;
        lease.authority.expires_at = expires_at.to_rfc3339();
    }

    #[derive(Debug)]
    struct CapturedRequest {
        path: String,
        authorization: String,
        api_key: Option<String>,
        conversation_id: String,
        request_id: String,
        account_id: Option<String>,
        body: Value,
    }

    async fn test_control_plane(captured: mpsc::UnboundedSender<CapturedRequest>) -> String {
        test_control_plane_with_authority(
            captured,
            AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5)),
        )
        .await
    }

    async fn test_control_plane_with_authority(
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
                            if request.method() == hyper::Method::HEAD && request.uri().path() == "/" {
                                return Ok::<_, Infallible>(Response::builder().status(StatusCode::NOT_FOUND)
                                    .body(Full::new(Bytes::new())).unwrap());
                            }
                            let path = request.uri().path().to_string();
                            let path_and_query = request
                                .uri()
                                .path_and_query()
                                .map(|value| value.as_str().to_string())
                                .unwrap_or_else(|| path.clone());
                            let headers = request.headers().clone();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            let response = match path.as_str() {
                                "/api/agent-auth/v2/authority" => {
                                    let authority_id =
                                        fixture.issued.fetch_add(1, Ordering::SeqCst) + 1;
                                    let gate = fixture.issuance_gate.lock().clone();
                                    if let Some(gate) = gate {
                                        gate.acquire().await.unwrap().forget();
                                    }
                                    if fixture.fail_issuance.load(Ordering::SeqCst) {
                                        return Ok::<_, Infallible>(Response::builder()
                                            .status(StatusCode::SERVICE_UNAVAILABLE)
                                            .header("retry-after", "1")
                                            .body(Full::new(Bytes::from_static(
                                                br#"{"error":"temporarily_unavailable"}"#,
                                            )))
                                            .unwrap());
                                    }
                                    json!({
                                        "contractVersion": 2,
                                        "token": format!("remote-agent-auth-authority-{authority_id}"),
                                        "tokenType": "Bearer",
                                        "authorityId": format!("authority-{authority_id}"),
                                        "principalId": fixture.principal.lock().clone(),
                                        "authorityScope": fixture.scope.lock().clone(),
                                        "expiresAt": fixture.expires_at.lock().to_rfc3339(),
                                    })
                                }
                                "/api/agent-auth/v2/responses" | "/api/agent-auth/v2/messages" => {
                                    fixture.forwarded.fetch_add(1, Ordering::SeqCst);
                                    captured
                                        .send(CapturedRequest {
                                            path: path_and_query,
                                            authorization: headers[AUTHORIZATION]
                                                .to_str()
                                                .unwrap()
                                                .to_string(),
                                            api_key: headers
                                                .get("x-api-key")
                                                .and_then(|value| value.to_str().ok())
                                                .map(str::to_string),
                                            conversation_id:
                                                headers["x-agent-auth-conversation-id"]
                                                    .to_str()
                                                    .unwrap()
                                                    .to_string(),
                                            request_id: headers["x-agent-auth-request-id"]
                                                .to_str()
                                                .unwrap()
                                                .to_string(),
                                            account_id: headers
                                                .get("x-agent-auth-account-id")
                                                .and_then(|value| value.to_str().ok())
                                                .map(str::to_string),
                                            body: serde_json::from_slice(&bytes).unwrap(),
                                        })
                                        .unwrap();
                                    let rejection = fixture.rejection.lock().clone();
                                    let gate = fixture.response_gate.lock().clone();
                                    if let Some(gate) = gate {
                                        gate.acquire().await.unwrap().forget();
                                    }
                                    if let Some((body, selected_account)) = rejection {
                                        let mut response = Response::builder().status(StatusCode::UNAUTHORIZED)
                                            .header("transfer-encoding", "chunked")
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
                            let mut response_builder = Response::builder();
                            if matches!(
                                path.as_str(),
                                "/api/agent-auth/v2/responses" | "/api/agent-auth/v2/messages"
                            ) {
                                response_builder = response_builder
                                    .header("x-agent-auth-selected-account-id", "account-actual");
                            }
                            Ok::<_, Infallible>(
                                response_builder
                                    .body(Full::new(Bytes::from(response.to_string())))
                                    .unwrap(),
                            )
                        }
                    });
                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        assert!(error.is_incomplete_message(), "unexpected fixture connection error: {error}");
                    }
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
                            if request.method() == hyper::Method::HEAD && request.uri().path() == "/" {
                                return Ok::<_, Infallible>(Response::builder().status(StatusCode::NOT_FOUND)
                                    .body(Full::new(Bytes::new())).unwrap());
                            }
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
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(
                                response.to_string(),
                            ))))
                        }
                    });
                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        assert!(error.is_incomplete_message(), "unexpected fixture connection error: {error}");
                    }
                });
            }
        });
        origin
    }

    async fn streaming_control_plane(captured: mpsc::UnboundedSender<CapturedRequest>) -> String {
        streaming_control_plane_with_status(captured, StatusCode::OK, None).await
    }

    async fn streaming_control_plane_with_status(
        captured: mpsc::UnboundedSender<CapturedRequest>,
        status: StatusCode,
        tail_gate: Option<Arc<Semaphore>>,
    ) -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                let tail_gate = tail_gate.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let captured = captured.clone();
                        let tail_gate = tail_gate.clone();
                        async move {
                            if request.method() == hyper::Method::HEAD && request.uri().path() == "/" {
                                return Ok::<_, Infallible>(Response::builder().status(StatusCode::NOT_FOUND)
                                    .body(Full::new(Bytes::new()).boxed()).unwrap());
                            }
                            let path = request.uri().path().to_string();
                            let headers = request.headers().clone();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            if path == "/api/agent-auth/v2/responses" {
                                captured
                                    .send(CapturedRequest {
                                        path: path.clone(),
                                        authorization: headers[AUTHORIZATION]
                                            .to_str()
                                            .unwrap()
                                            .to_string(),
                                        api_key: None,
                                        conversation_id: headers["x-agent-auth-conversation-id"]
                                            .to_str()
                                            .unwrap()
                                            .to_string(),
                                        request_id: headers["x-agent-auth-request-id"]
                                            .to_str()
                                            .unwrap()
                                            .to_string(),
                                        account_id: None,
                                        body: serde_json::from_slice(&bytes).unwrap(),
                                    })
                                    .unwrap();
                                let body = futures::stream::once(async move {
                                    let prefix: &'static [u8] = if status == StatusCode::UNAUTHORIZED {
                                        br#"{"error":"agent_auth_v2_authority_invalid"}"#
                                    } else {
                                        b"data: first\n\n"
                                    };
                                    Ok::<_, Infallible>(Frame::data(Bytes::from_static(prefix)))
                                }).chain(futures::stream::once(async move {
                                    match tail_gate {
                                        Some(gate) => gate.acquire().await.unwrap().forget(),
                                        None => futures::future::pending::<()>().await,
                                    }
                                    Ok::<_, Infallible>(Frame::data(Bytes::from_static(b" delayed-tail")))
                                }));
                                return Ok::<_, Infallible>(Response::builder().status(status)
                                    .header("x-fixture-stream", "preserved")
                                    .body(BodyExt::boxed(StreamBody::new(body))).unwrap());
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
                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        assert!(error.is_incomplete_message(), "unexpected fixture connection error: {error}");
                    }
                });
            }
        });
        origin
    }

    #[tokio::test]
    async fn distinct_routes_share_preparation_renewal_and_waiter_cancellation() {
        let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5));
        let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(captured_tx, fixture.clone()).await;
        let relay = InferenceRelay::start(
            ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap(),
        ).unwrap();
        let sessions = (0..16).map(|index| format!("shared-session-{index}")).collect::<Vec<_>>();
        let prepared = futures::future::join_all(sessions.iter().map(|session| {
            relay.prepare(session, HarnessId::Codex, Some("gpt-5.6-sol"), None)
        })).await;
        let routes = prepared.into_iter().map(|prepared| {
            let prepared = prepared.unwrap().unwrap();
            lock(&relay.inner.route_state).active.get(&prepared.token).unwrap().clone()
        }).collect::<Vec<_>>();
        relay.prepare("later-anthropic", HarnessId::ClaudeCode, Some("claude-opus-5"), Some("pinned-account"))
            .await.unwrap().unwrap();
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 1);

        let boundary = Utc::now() + TimeDelta::milliseconds(900);
        set_fixture_expiry(&fixture, &relay, boundary).await;
        let calls = futures::future::join_all(routes.iter().map(|route| relay.current_authority(route)));
        tokio::pin!(calls);
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut calls).await.is_err());
        routes[0].cancellation.cancel();
        *fixture.expires_at.lock() = Utc::now() + TimeDelta::minutes(5);
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
        let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(captured_tx, fixture.clone()).await;
        let relay = InferenceRelay::start(
            ScaffoldClient::new(origin, "project-1", token.clone()).unwrap(),
        ).unwrap();
        let prepared = relay.prepare("generation-session", HarnessId::Codex, None, None)
            .await.unwrap().unwrap();
        let route = lock(&relay.inner.route_state).active.get(&prepared.token).unwrap().clone();
        token.generation.fetch_add(1, Ordering::SeqCst);
        let renewed = relay.current_authority(&route).await.unwrap();
        assert_eq!(renewed.credential_generation, 1);
        assert_eq!(renewed.authority.authority_id, "authority-2");
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
        token.signed_out.store(true, Ordering::SeqCst);
        assert!(relay.current_authority(&route).await.is_err());
        assert!(relay.prepare("signed-out", HarnessId::Codex, None, None).await.is_err());
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn authority_rejection_is_exactly_once_and_stale_rejection_cannot_evict_new_grant() {
        let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5));
        let rejected_body = r#"{"error":"agent_auth_v2_authority_invalid"}"#;
        *fixture.rejection.lock() = Some((rejected_body.into(), false));
        let gate = Arc::new(Semaphore::new(0));
        *fixture.response_gate.lock() = Some(gate.clone());
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(captured_tx, fixture.clone()).await;
        let relay = InferenceRelay::start(
            ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap(),
        ).unwrap();
        let route = relay.prepare("rejected-route", HarnessId::Codex, None, None).await.unwrap().unwrap();
        let url = format!("{}/v1/responses", route.base_url);
        let http = reqwest::Client::new();
        let mut requests = tokio::task::JoinSet::new();
        for input in ["first", "stale"] {
            let request = http.post(&url).bearer_auth(&route.token).json(&json!({ "input": input }));
            requests.spawn(async move { request.send().await.unwrap() });
        }
        for _ in 0..2 {
            let sent = captured_rx.recv().await.unwrap();
            assert_eq!(sent.authorization, "Bearer remote-agent-auth-authority-1");
        }
        gate.add_permits(1);
        let rejected = requests.join_next().await.unwrap().unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(rejected.headers()["x-fixture-error"], "preserved");
        assert_eq!(rejected.text().await.unwrap(), rejected_body);
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 2);

        relay.prepare("independent-route", HarnessId::Codex, None, None).await.unwrap().unwrap();
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
        gate.add_permits(1);
        let stale = requests.join_next().await.unwrap().unwrap();
        assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(stale.text().await.unwrap(), rejected_body);
        *fixture.rejection.lock() = None;
        *fixture.response_gate.lock() = None;
        let subsequent = http.post(&url).bearer_auth(&route.token)
            .json(&json!({ "input": "independent-new-request" })).send().await.unwrap();
        assert_eq!(subsequent.status(), StatusCode::OK);
        let sent = captured_rx.recv().await.unwrap();
        assert_eq!(sent.authorization, "Bearer remote-agent-auth-authority-2");
        assert_eq!(sent.body["input"], "independent-new-request");
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 3);
        assert!(captured_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn stalled_401_probe_forwards_headers_prefix_and_remaining_body_without_eviction() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let tail_gate = Arc::new(Semaphore::new(0));
        let origin = streaming_control_plane_with_status(
            captured_tx, StatusCode::UNAUTHORIZED, Some(tail_gate.clone()),
        ).await;
        let relay = InferenceRelay::start(
            ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap(),
        ).unwrap();
        let route = relay.prepare("stalled-error", HarnessId::Codex, None, None).await.unwrap().unwrap();
        let request = reqwest::Client::new().post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token).json(&json!({ "input": "once" }));
        let mut response = tokio::time::timeout(Duration::from_secs(2), request.send())
            .await.expect("stalled authority error must not withhold response headers").unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["x-fixture-stream"], "preserved");
        let prefix = tokio::time::timeout(Duration::from_millis(500), response.chunk())
            .await.unwrap().unwrap().unwrap();
        assert_eq!(prefix, Bytes::from_static(br#"{"error":"agent_auth_v2_authority_invalid"}"#));
        relay.prepare("after-stalled-error", HarnessId::Codex, None, None).await.unwrap().unwrap();
        assert_eq!(relay.inner.authority.lock().await.lease.as_ref().unwrap().generation, 1);
        tail_gate.add_permits(1);
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), response.bytes()).await.unwrap().unwrap(),
            Bytes::from_static(b" delayed-tail"));
        assert_eq!(captured_rx.recv().await.unwrap().body["input"], "once");
        assert!(captured_rx.try_recv().is_err());
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
            *fixture.rejection.lock() = Some((body.clone(), selected_account));
            let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
            let origin = test_control_plane_with_authority(captured_tx, fixture.clone()).await;
            let relay = InferenceRelay::start(
                ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap(),
            ).unwrap();
            let route = relay.prepare("provider-error", HarnessId::Codex, None, None).await.unwrap().unwrap();
            let response = reqwest::Client::new().post(format!("{}/v1/responses", route.base_url))
                .bearer_auth(&route.token).json(&json!({ "input": "once" })).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(response.headers()["x-fixture-error"], "preserved");
            assert_eq!(response.text().await.unwrap(), body);
            assert_eq!(captured_rx.recv().await.unwrap().body["input"], "once");
            relay.prepare("after-provider-error", HarnessId::Codex, None, None).await.unwrap().unwrap();
            assert_eq!(fixture.issued.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn credential_change_during_issuance_cannot_publish_old_profile_authority() {
        for signed_out in [false, true] {
            let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5));
            let gate = Arc::new(Semaphore::new(0));
            *fixture.issuance_gate.lock() = Some(gate.clone());
            let token = Arc::new(ProfileToken::default());
            let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
            let origin = test_control_plane_with_authority(captured_tx, fixture.clone()).await;
            let relay = InferenceRelay::start(
                ScaffoldClient::new(origin, "project-1", token.clone()).unwrap(),
            ).unwrap();
            let preparing = {
                let relay = relay.clone();
                tokio::spawn(async move { relay.prepare("changing-profile", HarnessId::Codex, None, None).await })
            };
            tokio::time::timeout(Duration::from_secs(2), async {
                while fixture.issued.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            token.generation.fetch_add(1, Ordering::SeqCst);
            token.signed_out.store(signed_out, Ordering::SeqCst);
            gate.add_permits(1);
            assert!(preparing.await.unwrap().is_err());
            assert!(relay.inner.authority.lock().await.lease.is_none());
            assert!(lock(&relay.inner.route_state).active.is_empty());
            if !signed_out {
                *fixture.issuance_gate.lock() = None;
                relay.prepare("new-profile", HarnessId::Codex, None, None).await.unwrap().unwrap();
                assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
            }
        }
    }

    #[tokio::test]
    async fn profile_change_cannot_rebind_old_routes_or_retired_credentials() {
        for change_owner in [false, true] {
            let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5));
            let token = Arc::new(ProfileToken::default());
            let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
            let origin = test_control_plane_with_authority(captured_tx, fixture.clone()).await;
            let relay = InferenceRelay::start(
                ScaffoldClient::new(origin, "project-1", token.clone()).unwrap(),
            ).unwrap();
            let prepared = relay.prepare("profile-route", HarnessId::Codex, None, None).await.unwrap().unwrap();
            let old = lock(&relay.inner.route_state).active.get(&prepared.token).unwrap().clone();
            let retired = relay.prepare("retired-profile-route", HarnessId::Codex, None, None).await.unwrap().unwrap();
            relay.remove(&retired.token);
            token.generation.fetch_add(1, Ordering::SeqCst);
            *fixture.scope.lock() = "user:another-scope".into();
            if change_owner {
                *fixture.principal.lock() = "identity:another-owner".into();
            }
            let replacement = relay.prepare("retired-profile-route", HarnessId::Codex, None, None).await.unwrap().unwrap();
            assert_ne!(replacement.token, retired.token);
            assert!(relay.current_authority(&old).await.is_err());
            assert!(old.cancellation.is_cancelled());
            let response = reqwest::Client::new().get(format!("{}/v1/models", retired.base_url))
                .bearer_auth(&retired.token).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::GONE);
            assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
            assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn fixture_authority_waits_for_expiry_then_issues_once_for_concurrent_callers() {
        let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::seconds(20));
        let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(captured_tx, fixture.clone()).await;
        let relay = InferenceRelay::start(
            ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap(),
        ).unwrap();
        let prepared = relay.prepare("session-expiry", HarnessId::Codex, Some("gpt-5.6-sol"), None)
            .await.unwrap().unwrap();
        let route = lock(&relay.inner.route_state).active.get(&prepared.token).unwrap().clone();
        for _ in 0..8 {
            let authority = relay.current_authority(&route).await.unwrap();
            assert_eq!(authority.authority.authority_id, "authority-1");
        }
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 1);

        let boundary = Utc::now() + TimeDelta::milliseconds(900);
        set_fixture_expiry(&fixture, &relay, boundary).await;
        let calls = futures::future::join_all((0..16).map(|_| relay.current_authority(&route)));
        tokio::pin!(calls);
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut calls).await.is_err());
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 1);
        *fixture.expires_at.lock() = Utc::now() + TimeDelta::minutes(5);
        let renewed = tokio::time::timeout(Duration::from_secs(2), &mut calls).await.unwrap();
        assert!(Utc::now() >= boundary);
        for authority in renewed {
            let authority = authority.unwrap();
            assert_eq!(authority.authority.authority_id, "authority-2");
            assert_eq!(authority.authority.token, "remote-agent-auth-authority-2");
        }
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancelling_final_second_wait_does_not_issue_or_return_cached_authority() {
        let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::seconds(20));
        let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(captured_tx, fixture.clone()).await;
        let relay = InferenceRelay::start(
            ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap(),
        ).unwrap();
        let prepared = relay.prepare("session-cancel-expiry", HarnessId::Codex, Some("gpt-5.6-sol"), None)
            .await.unwrap().unwrap();
        let route = lock(&relay.inner.route_state).active.get(&prepared.token).unwrap().clone();
        set_fixture_expiry(&fixture, &relay, Utc::now() + TimeDelta::milliseconds(900)).await;
        let calls = futures::future::join_all((0..2).map(|_| relay.current_authority(&route)));
        tokio::pin!(calls);
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut calls).await.is_err());
        route.cancellation.cancel();
        let cancelled = tokio::time::timeout(Duration::from_millis(300), &mut calls).await.unwrap();
        assert!(cancelled.into_iter().all(|result| result.is_err()));
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 1);
        assert!(relay.current_authority(&route).await.is_err());
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn signed_out_profile_fences_cached_authority_and_both_wait_paths() {
        for wait in ["cached", "mutex", "expiry"] {
            let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::seconds(20));
            let token = Arc::new(ProfileToken::default());
            let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
            let origin = test_control_plane_with_authority(captured_tx, fixture.clone()).await;
            let relay = InferenceRelay::start(
                ScaffoldClient::new(origin, "project-1", token.clone()).unwrap(),
            ).unwrap();
            let prepared = relay.prepare("session-profile", HarnessId::Codex, Some("gpt-5.6-sol"), None)
                .await.unwrap().unwrap();
            let route = lock(&relay.inner.route_state).active.get(&prepared.token).unwrap().clone();
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
    async fn transient_issuance_cools_down_concurrent_loopback_requests_without_forwarding() {
        let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::seconds(20));
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(captured_tx, fixture.clone()).await;
        let relay = InferenceRelay::start(
            ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap(),
        ).unwrap();
        let prepared = relay.prepare("session-cooldown", HarnessId::Codex, Some("gpt-5.6-sol"), None)
            .await.unwrap().unwrap();
        let route = lock(&relay.inner.route_state).active.get(&prepared.token).unwrap().clone();
        let sibling = relay.prepare("cooldown-sibling", HarnessId::Codex, None, None).await.unwrap().unwrap();
        set_fixture_expiry(&fixture, &relay, Utc::now() - TimeDelta::seconds(1)).await;
        fixture.fail_issuance.store(true, Ordering::SeqCst);
        let http = reqwest::Client::new();
        let url = format!("{}/v1/responses", prepared.base_url);
        for _ in 0..2 {
            let requests = (0..8).map(|index| http.post(&url)
                .bearer_auth(if index % 2 == 0 { &prepared.token } else { &sibling.token })
                .json(&json!({ "model": "gpt-5.6-sol", "input": "cooldown" })).send());
            let responses = tokio::time::timeout(Duration::from_secs(2), futures::future::join_all(requests))
                .await.unwrap();
            for response in responses {
                let response = response.unwrap();
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(response.headers()["retry-after"], "1");
                assert_eq!(response.json::<Value>().await.unwrap()["error"], "agent_auth_unavailable");
            }
            assert!(relay.prepare("cooldown-prepare", HarnessId::Codex, None, None).await.is_err());
            assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
            assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 0);
            assert!(captured_rx.try_recv().is_err());
        }

        let (started, delay) = relay.inner.authority.lock().await.retry_at.unwrap();
        relay.inner.authority.lock().await.retry_at = Some((Instant::now(), Duration::from_secs(u64::MAX)));
        let error = relay.current_authority(&route).await.err().unwrap();
        let remaining = error.retry_after.unwrap();
        assert!(remaining >= Duration::from_secs(u64::MAX - 1));
        assert!(remaining <= Duration::from_secs(u64::MAX));
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 0);
        relay.inner.authority.lock().await.retry_at = Some((started, delay));
        tokio::time::sleep(delay.saturating_sub(started.elapsed()) + Duration::from_millis(20)).await;
        fixture.fail_issuance.store(false, Ordering::SeqCst);
        *fixture.expires_at.lock() = Utc::now() + TimeDelta::minutes(5);
        let response = http.post(&url).bearer_auth(&prepared.token)
            .json(&json!({ "model": "gpt-5.6-sol", "input": "resumed" })).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 3);
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 1);
        let forwarded = captured_rx.recv().await.unwrap();
        assert_eq!(forwarded.authorization, "Bearer remote-agent-auth-authority-3");
        assert_eq!(forwarded.body["input"], "resumed");
        assert!(relay.inner.authority.lock().await.retry_at.is_none());
    }

    #[test]
    fn refresh_failure_preserves_auth_stage_and_status_without_api_body_secrets() {
        use crate::scaffold::{ScaffoldApiError, ScaffoldApiErrorDisplay, ScaffoldError};

        let auth = AuthorityRefreshError::scaffold(
            "auth_unavailable",
            &ScaffoldError::AuthTemporarilyUnavailable(
                "Scaffold control session verification returned HTTP 429".into(),
            ),
        );
        assert_eq!(auth.reason, "auth_unavailable");
        assert_eq!(auth.cause.as_deref(), Some(
            "scaffold_auth_temporarily_unavailable: Scaffold control session verification returned HTTP 429",
        ));

        let api = AuthorityRefreshError::scaffold(
            "authority_issuance_failed",
            &ScaffoldError::Api(ScaffoldApiErrorDisplay(ScaffoldApiError {
                status: 429,
                code: "secret-token-in-upstream-code".into(),
                message: Some("secret-principal-and-scope-in-upstream-message".into()),
                retry_after: Some(Duration::from_secs(1)),
            })),
        );
        assert_eq!(api.reason, "authority_issuance_failed");
        assert_eq!(api.cause.as_deref(), Some("scaffold_api_error:429"));
        assert!(!format!("{api:?}").contains("secret-"));

        let invalid = AuthorityRefreshError::scaffold(
            "authority_issuance_failed",
            &ScaffoldError::InvalidResponse("secret-payload-in-parser-error".into()),
        );
        assert_eq!(invalid.cause.as_deref(), Some("scaffold_response_invalid"));
        assert!(!format!("{invalid:?}").contains("secret-"));
    }

    fn observation_fixture(epoch: u64) -> AgentInferenceAuthority {
        use base64::Engine as _;

        // An unsigned diagnostic payload fixture, not evidence of issuer renewal.
        let claims = json!({
            "version": 2,
            "authorityId": "private-authority-id",
            "principalId": "private-principal-id",
            "authorityScope": "private-authority-scope",
            "authorityEpoch": epoch,
            "issuedAt": 1_700_000_000_i64,
            "expiresAt": 1_700_000_060_i64,
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        AgentInferenceAuthority {
            contract_version: 2,
            token: format!("ia2.{payload}.private-signature"),
            token_type: "Bearer".into(),
            authority_id: "private-authority-id".into(),
            principal_id: "private-principal-id".into(),
            authority_scope: Some("private-authority-scope".into()),
            expires_at: DateTime::<Utc>::from_timestamp_millis(1_700_000_060_789).unwrap().to_rfc3339(),
        }
    }

    #[test]
    fn authority_observation_requires_bounded_payload_and_matching_grant() {
        let authority = observation_fixture(7);
        let observation = authority_observation(&authority).unwrap();
        assert_eq!(observation.authority_id_digest, format!("{:x}", Sha256::digest(b"private-authority-id")));
        assert_eq!(observation.epoch, 7);
        assert_eq!(observation.issued_at, 1_700_000_000);
        assert_eq!(observation.expires_at, 1_700_000_060);
        for field in ["version", "id", "principal", "scope", "missing_scope", "expiry"] {
            let mut mismatched = authority.clone();
            match field {
                "version" => mismatched.contract_version = 1,
                "id" => mismatched.authority_id = "other-authority".into(),
                "principal" => mismatched.principal_id = "other-owner".into(),
                "scope" => mismatched.authority_scope = Some("other-scope".into()),
                "missing_scope" => mismatched.authority_scope = None,
                "expiry" => mismatched.expires_at = "2023-11-14T22:14:21Z".into(),
                _ => unreachable!(),
            }
            assert!(authority_observation(&mismatched).is_none(), "{field}");
        }
        assert!(authority_observation(&observation_fixture(0)).is_none());
        let payload = authority.token.split('.').nth(1).unwrap();
        for token in [
            String::new(),
            "ia2".into(),
            "ia2.%%%.signature".into(),
            "ia2.e30.signature".into(),
            "ia2._w.signature".into(),
            format!("ia2.{payload}"),
            format!("ia1.{payload}.signature"),
            format!("ia2.{payload}."),
            format!("ia2.{payload}.signature.extra"),
            format!("ia2.{payload}.{}", "s".repeat(8192)),
            format!("ia2.{}.signature", "a".repeat(1_000_000)),
        ] {
            let mut malformed = authority.clone();
            malformed.token = token;
            assert!(authority_observation(&malformed).is_none());
        }
    }

    #[test]
    fn authority_diagnostics_emit_only_bounded_nonsecret_observations() {
        let logs = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer({
                let logs = logs.clone();
                move || LogWriter(logs.clone())
            })
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let previous = observation_fixture(7);
        let current = observation_fixture(8);
        observe_authority("fixture_initial", &previous, None);
        observe_authority("fixture_reissue", &current, Some(&previous));
        let mut malformed = current.clone();
        malformed.token = "ia2.malformed.private-signature".into();
        observe_authority("fixture_invalid", &malformed, Some(&previous));
        malformed.token = format!("ia2.{}.private-signature", "a".repeat(1_000_000));
        observe_authority("fixture_oversized", &malformed, Some(&previous));
        let mut mismatched = current.clone();
        mismatched.principal_id = "different-private-principal".into();
        observe_authority("fixture_mismatch", &mismatched, Some(&previous));
        let logs = String::from_utf8(logs.lock().clone()).unwrap();
        assert_eq!(logs.lines().count(), 2, "{logs}");
        for field in ["authority_id_digest=", "epoch=8", "issued_at=1700000000", "expires_at=1700000060", "observed_at=", "previous_epoch=7", "previous_expiry=1700000060"] {
            assert!(logs.contains(field), "missing {field}: {logs}");
        }
        for secret in [
            current.authority_id.as_str(),
            current.principal_id.as_str(),
            current.authority_scope.as_deref().unwrap(),
            current.token.as_str(),
            current.token.split('.').nth(1).unwrap(),
            "principal", "scope", "token", "signature", "payload", "fixture_invalid",
        ] {
            assert!(!logs.contains(secret), "diagnostics leaked {secret}");
        }
    }

    #[tokio::test]
    async fn renewal_cannot_rebind_an_existing_route_to_another_owner() {
        let (captured_tx, _) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let relay = InferenceRelay::start(
            ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap()
        ).unwrap();
        let prepared = relay.prepare("session-owner", HarnessId::Codex, Some("gpt-5.6-sol"), None)
            .await.unwrap().unwrap();
        let route = lock(&relay.inner.route_state).active.get(&prepared.token).unwrap().clone();
        relay.inner.authority.lock().await.lease = None;
        let changed_owner = Route {
            request: route.request.clone(),
            local_session_id: None,
            owner_subject: "identity:someone-else".into(),
            cancellation: zeron_harness::CancellationToken::new(),
            authority_scope: route.authority_scope.clone(),
        };
        assert!(relay.current_authority(&changed_owner).await.is_err());
        assert!(changed_owner.cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn renewal_cannot_rebind_an_existing_route_to_another_scope_for_same_owner() {
        let fixture = AuthorityFixture::new(Utc::now() + TimeDelta::minutes(5));
        let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(captured_tx, fixture.clone()).await;
        let relay = InferenceRelay::start(
            ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap(),
        ).unwrap();
        let prepared = relay.prepare("session-scope", HarnessId::Codex, Some("gpt-5.6-sol"), None)
            .await.unwrap().unwrap();
        let route = lock(&relay.inner.route_state).active.get(&prepared.token).unwrap().clone();
        relay.inner.authority.lock().await.lease = None;
        let changed_scope = Route {
            request: route.request.clone(),
            local_session_id: None,
            owner_subject: route.owner_subject.clone(),
            authority_scope: Some("user:previous-profile-scope".into()),
            cancellation: zeron_harness::CancellationToken::new(),
        };
        let error = relay.current_authority(&changed_scope).await.err().unwrap();
        assert_eq!(error.reason, "route_scope_changed");
        assert!(changed_scope.cancellation.is_cancelled());
        assert_eq!(fixture.issued.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn issues_authority_streams_through_loopback_and_removes_locally() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare("session-1", HarnessId::Codex, Some("gpt-5.6-sol"), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.provider, "openai");
        assert_eq!(route.token.len(), 64);

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

        let response = http
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .header("x-request-id", "request-1")
            .json(&json!({ "model": "gpt-5.6-sol", "input": "hello" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({ "ok": true })
        );
        let captured = captured_rx.recv().await.unwrap();
        assert_eq!(
            captured.authorization,
            "Bearer remote-agent-auth-authority-1"
        );
        assert_eq!(captured.conversation_id, "session-1");
        assert_eq!(captured.request_id, "request-1");
        assert_eq!(captured.account_id, None);
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
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare(
                "session-large-body",
                HarnessId::Codex,
                Some("gpt-5.6-sol"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
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
    fn projects_only_local_import_ids_to_stable_agent_auth_uuids() {
        let local_id = "local-chat-b5b85d0f52a29e39da7656ab";
        let projected = agent_auth_logical_session_id(local_id).into_owned();
        let parsed = uuid::Uuid::parse_str(&projected).unwrap();

        assert_eq!(projected, agent_auth_logical_session_id(local_id));
        assert_eq!(parsed.as_bytes()[6] >> 4, 8);
        assert_eq!(parsed.as_bytes()[8] & 0xc0, 0x80);
        assert_eq!(agent_auth_logical_session_id("session-1"), "session-1");
    }

    #[tokio::test]
    async fn projects_local_import_id_and_retires_its_local_token() {
        let local_id = "local-chat-b5b85d0f52a29e39da7656ab";
        let projected = agent_auth_logical_session_id(local_id).into_owned();

        let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare(
                local_id,
                HarnessId::Omp,
                Some("openai-codex/gpt-5.6-sol"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        {
            let state = lock(&relay.inner.route_state);
            let active = state.active.get(&route.token).unwrap();
            assert_eq!(active.request.logical_session_id, projected);
            assert_eq!(active.session_id(), local_id);
        }

        relay.remove(&route.token);
        let removed = reqwest::Client::new()
            .get(format!("{}/v1/models", route.base_url))
            .bearer_auth(&route.token)
            .send()
            .await
            .unwrap();
        assert_eq!(removed.status(), reqwest::StatusCode::GONE);
    }

    #[tokio::test]
    async fn rebinds_retired_local_credential_to_the_next_route_for_the_same_session() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let first = relay
            .prepare(
                "persistent-session",
                HarnessId::Omp,
                Some("anthropic/claude-opus-5"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let original_token = first.token.clone();

        relay.remove(&original_token);

        let replacement = relay
            .prepare(
                "persistent-session",
                HarnessId::Omp,
                Some("anthropic/claude-opus-5"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replacement.token, original_token);

        let response = reqwest::Client::new()
            .post(format!("{}/v1/messages", replacement.base_url))
            .bearer_auth(&original_token)
            .header("x-request-id", "persistent-worker-request")
            .json(&json!({ "model": "claude-opus-5", "messages": [] }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            captured_rx.recv().await.unwrap().conversation_id,
            "persistent-session"
        );

        let unrelated = relay
            .prepare(
                "unrelated-session",
                HarnessId::Omp,
                Some("anthropic/claude-opus-5"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(unrelated.token, original_token);
    }

    #[tokio::test]
    async fn provider_switch_does_not_rebind_the_retired_credential() {
        let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let anthropic = relay
            .prepare(
                "provider-switch-session",
                HarnessId::Omp,
                Some("anthropic/claude-opus-5"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        relay.remove(&anthropic.token);
        let openai = relay
            .prepare(
                "provider-switch-session",
                HarnessId::Omp,
                Some("openai-codex/gpt-5.6-sol"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(openai.token, anthropic.token);

        let response = reqwest::Client::new()
            .get(format!("{}/v1/models", anthropic.base_url))
            .bearer_auth(&anthropic.token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::GONE);
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({ "error": "inference_route_expired", "restart_required": false })
        );
    }

    #[tokio::test]
    async fn retired_worker_credential_uses_the_replacement_parent_authority() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let first = relay
            .prepare(
                "persistent-session",
                HarnessId::Omp,
                Some("anthropic/claude-opus-5"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        relay.remove(&first.token);
        {
            let mut state = lock(&relay.inner.route_state);
            state.retired.insert(
                "persistent-worker-token".into(),
                RetiredRoute {
                    logical_session_id: "persistent-session".into(),
                    local_session_id: None,
                    owner_subject: "identity:owner-1".into(),
                    authority_scope: Some("user:identity:owner-1".into()),
                    provider: "anthropic".into(),
                    retired_at: Instant::now() - Duration::from_secs(1),
                },
            );
        }

        let parent = relay
            .prepare(
                "persistent-session",
                HarnessId::Omp,
                Some("anthropic/claude-opus-5"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(parent.token, first.token);

        let rebound = reqwest::Client::new()
            .post(format!("{}/v1/messages", parent.base_url))
            .bearer_auth("persistent-worker-token")
            .header("x-request-id", "rebound-worker-request")
            .json(&json!({ "model": "claude-opus-5", "messages": [] }))
            .send()
            .await
            .unwrap();
        assert_eq!(rebound.status(), reqwest::StatusCode::OK);
        let captured = captured_rx.recv().await.unwrap();
        assert_eq!(
            captured.authorization,
            "Bearer remote-agent-auth-authority-1"
        );
        assert_eq!(captured.conversation_id, "persistent-session");

        relay.remove(&parent.token);
        let retired = reqwest::Client::new()
            .get(format!("{}/v1/models", parent.base_url))
            .bearer_auth("persistent-worker-token")
            .send()
            .await
            .unwrap();
        assert_eq!(retired.status(), reqwest::StatusCode::GONE);
    }

    #[tokio::test]
    async fn route_removal_ends_an_active_inference_stream() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = streaming_control_plane(captured_tx).await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare("session-1", HarnessId::Codex, Some("gpt-5.6-sol"), None)
            .await
            .unwrap()
            .unwrap();
        let route_to_remove = route.clone();
        let http = reqwest::Client::new();
        let response = http
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .header("x-request-id", "request-cancel")
            .json(&json!({ "model": "gpt-5.6-sol", "input": "hello" }))
            .send()
            .await
            .unwrap();
        captured_rx.recv().await.unwrap();

        relay.remove(&route_to_remove.token);
        assert_eq!(
            response.bytes().await.unwrap(),
            Bytes::from_static(b"data: first\n\n")
        );
    }

    #[tokio::test]
    async fn authenticates_anthropic_sdk_requests_without_forwarding_the_loopback_token() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare(
                "session-anthropic",
                HarnessId::ClaudeCode,
                Some("claude-opus-5"),
                None,
            )
            .await
            .unwrap()
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
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({ "ok": true })
        );

        let captured = captured_rx.recv().await.unwrap();
        assert_eq!(captured.path, "/api/agent-auth/v2/messages?beta=true");
        assert_eq!(
            captured.authorization,
            "Bearer remote-agent-auth-authority-1"
        );
        assert_eq!(captured.api_key, None);
        assert_eq!(captured.conversation_id, "session-anthropic");
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

        let mut empty_scope = authority;
        empty_scope.authority_scope = Some(String::new());
        assert!(validate_authority(&empty_scope).is_err());
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
        let anthropic = local_relay_token("anthropic");
        assert!(anthropic.starts_with("sk-ant-oat01-"));
        assert!(!anthropic.chars().any(char::is_whitespace));
        assert!(!local_relay_token("openai").starts_with("sk-ant-oat01-"));
    }

    #[test]
    fn normalizes_only_direct_openai_and_anthropic_model_selectors() {
        assert_eq!(
            inference_binding(HarnessId::Omp, Some("openai-codex/gpt-5.6-sol")),
            Some(("openai", "gpt-5.6-sol".into()))
        );
        assert_eq!(
            inference_binding(HarnessId::Omp, Some("anthropic/claude-opus-5")),
            Some(("anthropic", "claude-opus-5".into()))
        );
        assert_eq!(
            inference_binding(HarnessId::Omp, Some("openai/gpt-5.6-sol")),
            Some(("openai", "gpt-5.6-sol".into()))
        );
        assert_eq!(
            inference_binding(
                HarnessId::Omp,
                Some("prime-inference/x-ai/grok-4.20")
            ),
            None
        );
        assert_eq!(
            inference_binding(HarnessId::Omp, Some("prime-inference/moonshotai/kimi-k3")),
            None
        );
        assert_eq!(
            inference_binding(HarnessId::ClaudeCode, Some("opus")),
            Some(("anthropic", "opus".into()))
        );
    }

    #[test]
    fn acp_provider_model_does_not_request_shared_inference_authority() {
        assert!(
            inference_route_request(
                "session-acp",
                HarnessId::Hermes,
                Some("anthropic/claude-fable-5-1"),
                None,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn route_requests_preserve_automatic_and_pinned_account_selection() {
        let automatic =
            inference_route_request("session-1", HarnessId::Codex, Some("gpt-5.6-sol"), None)
                .unwrap()
                .unwrap();
        assert_eq!(automatic.logical_session_id, "session-1");
        assert_eq!(automatic.provider, "openai");
        assert_eq!(automatic.model, "gpt-5.6-sol");
        assert_eq!(automatic.requested_account_id, None);

        let pinned = inference_route_request(
            "session-1",
            HarnessId::ClaudeCode,
            Some("claude-opus-5"),
            Some("opaque-account-id"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(pinned.provider, "anthropic");
        assert_eq!(
            pinned.requested_account_id.as_deref(),
            Some("opaque-account-id")
        );
    }

    #[test]
    fn resolves_deterministic_defaults_after_local_credentials_are_migrated() {
        assert_eq!(
            inference_binding(HarnessId::ClaudeCode, Some("default")),
            Some(("anthropic", "claude-opus-5".into()))
        );
        assert_eq!(
            inference_binding(HarnessId::Codex, None),
            Some(("openai", "gpt-6-sol".into()))
        );
        assert_eq!(inference_binding(HarnessId::Omp, None), None);
        assert_eq!(
            inference_binding(HarnessId::Omp, Some("default")),
            None
        );
    }
}
