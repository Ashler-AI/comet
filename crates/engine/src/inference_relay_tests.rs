#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::AgentInferenceAuthority;
    use async_trait::async_trait;
    use http_body_util::{BodyExt, Full, StreamBody};
    use hyper::{body::Frame, body::Incoming, service::service_fn};
    use serde_json::Value;
    use futures::stream;
    use std::io;
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
        cancellation: comet_harness::CancellationToken,
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
            comet_harness::CancellationToken::new(),
            "request-complete",
        );
        assert_eq!(complete.next().await.unwrap().unwrap(), Bytes::from_static(b"abc"));
        assert!(complete.next().await.is_none());

        let cancellation = comet_harness::CancellationToken::new();
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
            comet_harness::CancellationToken::new(),
            "request-failed",
        );
        assert_eq!(failed.next().await.unwrap().unwrap_err().to_string(), "body closed");

        drop(InstrumentedRelayStream::new(
            Box::pin(stream::pending()),
            comet_harness::CancellationToken::new(),
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
        assert!(logs.lines().any(|line| line.contains("request-failed") && line.contains("body closed")));
    }

    struct StaticToken;

    #[async_trait]
    impl comet_rpc::TokenSource for StaticToken {
        async fn token(&self) -> Option<String> {
            Some("comet-access-token".into())
        }
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
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let next_authority = Arc::new(AtomicU64::new(0));
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                let next_authority = next_authority.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let captured = captured.clone();
                        let next_authority = next_authority.clone();
                        async move {
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
                                    let authority_id = next_authority.fetch_add(1, Ordering::SeqCst) + 1;
                                    json!({
                                        "contractVersion": 2,
                                        "token": format!("remote-agent-auth-authority-{authority_id}"),
                                        "tokenType": "Bearer",
                                        "authorityId": format!("authority-{authority_id}"),
                                        "principalId": "identity:owner-1",
                                        "authorityScope": "user:identity:owner-1",
                                        "expiresAt": (Utc::now() + TimeDelta::minutes(5)).to_rfc3339(),
                                    })
                                }
                                "/api/agent-auth/v2/responses" | "/api/agent-auth/v2/messages" => {
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
                                            conversation_id: headers["x-agent-auth-conversation-id"]
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
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                        .unwrap();
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
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(
                                response.to_string(),
                            ))))
                        }
                    });
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                        .unwrap();
                });
            }
        });
        origin
    }

    async fn streaming_control_plane(
        captured: mpsc::UnboundedSender<CapturedRequest>,
    ) -> String {
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
                                let body = futures::stream::once(async {
                                    Ok::<_, Infallible>(Frame::data(Bytes::from_static(
                                        b"data: first\n\n",
                                    )))
                                })
                                .chain(futures::stream::pending());
                                return Ok::<_, Infallible>(Response::new(BodyExt::boxed(
                                    StreamBody::new(body),
                                )));
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
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                        .unwrap();
                });
            }
        });
        origin
    }

    #[tokio::test]
    async fn issues_authority_streams_through_loopback_and_removes_locally() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = test_control_plane(captured_tx).await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare(
                "session-1",
                HarnessId::Codex,
                Some("gpt-5.6-sol"),
                None,
            )
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
        assert_eq!(
            relay.selected_account_id("session-1").as_deref(),
            Some("account-actual")
        );

        let captured = captured_rx.recv().await.unwrap();
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-authority-1");
        assert_eq!(captured.conversation_id, "session-1");
        assert_eq!(captured.request_id, "request-1");
        assert_eq!(captured.account_id, None);
        assert_eq!(captured.body["input"], "hello");

        let mut expired_routes = relay.subscribe_expired_routes();
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
        assert_eq!(
            expired_routes.recv().await.unwrap(),
            ExpiredRoute {
                logical_session_id: "session-1".into(),
                lifecycle_epoch: 1,
            }
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
    async fn projects_local_import_id_and_restores_it_on_expiration() {
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

        let mut expired_routes = relay.subscribe_expired_routes();
        relay.remove(&route.token);
        let removed = reqwest::Client::new()
            .get(format!("{}/v1/models", route.base_url))
            .bearer_auth(&route.token)
            .send()
            .await
            .unwrap();
        assert_eq!(removed.status(), reqwest::StatusCode::GONE);
        assert_eq!(
            expired_routes.recv().await.unwrap(),
            ExpiredRoute {
                logical_session_id: local_id.into(),
                lifecycle_epoch: 1,
            }
        );
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
        assert_eq!(captured_rx.recv().await.unwrap().conversation_id, "persistent-session");

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

        let mut expired_routes = relay.subscribe_expired_routes();
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
        assert!(matches!(
            expired_routes.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
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
                    provider: "anthropic".into(),
                    lifecycle_epoch: 1,
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

        let mut expired_routes = relay.subscribe_expired_routes();
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
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-authority-2");
        assert_eq!(captured.conversation_id, "persistent-session");
        assert!(matches!(
            expired_routes.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

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
            .prepare(
                "session-1",
                HarnessId::Codex,
                Some("gpt-5.6-sol"),
                None,
            )
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
        assert_eq!(response.bytes().await.unwrap(), Bytes::from_static(b"data: first\n\n"));
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
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-authority-1");
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
            inference_binding(HarnessId::PrimeAgent, Some("anthropic/claude-opus-5")),
            Some(("anthropic", "claude-opus-5".into()))
        );
        assert_eq!(
            inference_binding(HarnessId::PrimeAgent, Some("openai/gpt-5.6-sol")),
            Some(("openai", "gpt-5.6-sol".into()))
        );
        assert_eq!(
            inference_binding(
                HarnessId::PrimeAgent,
                Some("prime-inference/x-ai/grok-4.20")
            ),
            None
        );
        assert_eq!(
            inference_binding(
                HarnessId::Omp,
                Some("prime-inference/moonshotai/kimi-k3")
            ),
            None
        );
        assert_eq!(
            inference_binding(HarnessId::ClaudeCode, Some("opus")),
            Some(("anthropic", "opus".into()))
        );
    }

    #[test]
    fn route_requests_preserve_automatic_and_pinned_account_selection() {
        let automatic =
            inference_route_request("session-1", HarnessId::Codex, Some("gpt-5.6-sol"), None, 1)
                .unwrap()
                .unwrap();
        assert_eq!(automatic.logical_session_id, "session-1");
        assert_eq!(automatic.provider, "openai");
        assert_eq!(automatic.model, "gpt-5.6-sol");
        assert_eq!(automatic.requested_account_id, None);
        assert_eq!(automatic.lifecycle_epoch, 1);

        let pinned = inference_route_request(
            "session-1",
            HarnessId::ClaudeCode,
            Some("claude-opus-5"),
            Some("opaque-account-id"),
            2,
        )
        .unwrap()
        .unwrap();
        assert_eq!(pinned.provider, "anthropic");
        assert_eq!(pinned.requested_account_id.as_deref(), Some("opaque-account-id"));
        assert_eq!(pinned.lifecycle_epoch, 2);
    }

    #[test]
    fn resolves_deterministic_defaults_after_local_credentials_are_migrated() {
        assert_eq!(
            inference_binding(HarnessId::ClaudeCode, Some("default")),
            Some(("anthropic", "claude-opus-5".into()))
        );
        assert_eq!(
            inference_binding(HarnessId::Codex, None),
            Some(("openai", "gpt-5.6-sol".into()))
        );
        assert_eq!(inference_binding(HarnessId::Omp, None), None);
        assert_eq!(
            inference_binding(HarnessId::PrimeAgent, Some("default")),
            None
        );
    }
}
