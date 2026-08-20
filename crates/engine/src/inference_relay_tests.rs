#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::AgentInferenceGrantBinding;
    use async_trait::async_trait;
    use http_body_util::{BodyExt, Full, StreamBody};
    use hyper::{body::Frame, body::Incoming, service::service_fn};
    use serde_json::Value;
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
        authorization: String,
        api_key: Option<String>,
        owner_subject: String,
        session_id: String,
        request_id: String,
        routing_mode: String,
        requested_account_id: Option<String>,
        body: Value,
    }

    async fn test_control_plane(
        captured: mpsc::UnboundedSender<CapturedRequest>,
        revoked: Arc<std::sync::atomic::AtomicBool>,
    ) -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                let revoked = revoked.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let captured = captured.clone();
                        let revoked = revoked.clone();
                        async move {
                            let path = request.uri().path().to_string();
                            let headers = request.headers().clone();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            let response = match path.as_str() {
                                "/api/agent-auth/grants" => {
                                    let input: Value = serde_json::from_slice(&bytes).unwrap();
                                    let expires_at =
                                        (Utc::now() + TimeDelta::minutes(5)).to_rfc3339();
                                    json!({
                                        "token": format!("remote-agent-auth-grant-{}", input["lifecycleEpoch"]),
                                        "expiresAt": expires_at,
                                        "binding": {
                                            "ownerSubject": "owner-1",
                                            "logicalSessionId": input["logicalSessionId"],
                                            "provider": input["provider"],
                                            "model": input["model"],
                                            "harness": input["harness"],
                                            "routingMode": input["routingMode"],
                                            "requestedAccountId": input["requestedAccountId"],
                                            "source": "comet-local",
                                            "lifecycleEpoch": input["lifecycleEpoch"],
                                            "environment": "local",
                                            "backend": "oauth",
                                            "accountId": "account-1",
                                            "accountGeneration": 1,
                                        }
                                    })
                                }
                                "/api/agent-auth/v1/responses"
                                | "/api/agent-auth/v1/messages" => {
                                    captured
                                        .send(CapturedRequest {
                                            authorization: headers[AUTHORIZATION]
                                                .to_str()
                                                .unwrap()
                                                .to_string(),
                                            api_key: headers
                                                .get("x-api-key")
                                                .and_then(|value| value.to_str().ok())
                                                .map(str::to_string),
                                            owner_subject: headers
                                                ["x-agent-auth-owner-subject"]
                                                .to_str()
                                                .unwrap()
                                                .to_string(),
                                            session_id: headers["x-agent-auth-session-id"]
                                                .to_str()
                                                .unwrap()
                                                .to_string(),
                                            request_id: headers["x-agent-auth-request-id"]
                                                .to_str()
                                                .unwrap()
                                                .to_string(),
                                            routing_mode: headers["x-agent-auth-routing-mode"]
                                                .to_str()
                                                .unwrap()
                                                .to_string(),
                                            requested_account_id: headers
                                                .get("x-agent-auth-requested-account-id")
                                                .and_then(|value| value.to_str().ok())
                                                .map(str::to_string),
                                            body: serde_json::from_slice(&bytes).unwrap(),
                                        })
                                        .unwrap();
                                    json!({ "ok": true })
                                }
                                "/api/agent-auth/grants/revoke" => {
                                    revoked.store(true, std::sync::atomic::Ordering::SeqCst);
                                    json!({ "ok": true })
                                }
                                other => panic!("unexpected control-plane path {other}"),
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
                            let expires_at =
                                (Utc::now() + TimeDelta::minutes(5)).to_rfc3339();
                            let (status, response) = match path.as_str() {
                                "/api/agent-auth/grants" => {
                                    let _ = request.into_body().collect().await.unwrap();
                                    (
                                        StatusCode::CREATED,
                                        json!({
                                            "token": "remote-large-body-account",
                                            "expiresAt": expires_at,
                                            "binding": {
                                                "ownerSubject": "owner-1",
                                                "logicalSessionId": "session-large-body",
                                                "provider": "openai",
                                                "model": "gpt-5.6-sol",
                                                "harness": "codex",
                                                "routingMode": "automatic",
                                                "requestedAccountId": null,
                                                "source": "comet-local",
                                                "lifecycleEpoch": 1,
                                                "environment": "local",
                                                "backend": "oauth",
                                                "accountId": "account-1",
                                                "accountGeneration": 1,
                                            }
                                        }),
                                    )
                                }
                                "/api/agent-auth/v1/responses" => {
                                    let mut body = request.into_body().into_data_stream();
                                    let mut observed = 0_u64;
                                    while let Some(chunk) = body.next().await {
                                        observed += chunk.unwrap().len() as u64;
                                    }
                                    captured.send(observed).unwrap();
                                    (StatusCode::OK, json!({ "ok": true }))
                                }
                                other => panic!("unexpected counting control-plane path {other}"),
                            };
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(status)
                                    .header(CONTENT_TYPE, "application/json")
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

    async fn rebind_control_plane(captured: mpsc::UnboundedSender<Value>) -> String {
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
                            assert_eq!(request.uri().path(), "/api/agent-auth/grants/rebind");
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            captured
                                .send(serde_json::from_slice(&bytes).unwrap())
                                .unwrap();
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"{}"))))
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


    #[derive(Debug)]
    struct FailoverCapture {
        path: String,
        authorization: String,
        request_id: Option<String>,
        routing_mode: String,
        requested_account_id: Option<String>,
        body: Value,
    }

    async fn failover_control_plane(
        captured: mpsc::UnboundedSender<FailoverCapture>,
        upstream_responses: Vec<(StatusCode, Value)>,
    ) -> String {
        failover_control_plane_with_content_type(
            captured,
            upstream_responses,
            "application/json",
        )
        .await
    }

    async fn failover_control_plane_with_content_type(
        captured: mpsc::UnboundedSender<FailoverCapture>,
        upstream_responses: Vec<(StatusCode, Value)>,
        content_type: &'static str,
    ) -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let upstream_responses = Arc::new(upstream_responses);
        let next_upstream_response =
            Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                let upstream_responses = upstream_responses.clone();
                let next_upstream_response = next_upstream_response.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let captured = captured.clone();
                        let upstream_responses = upstream_responses.clone();
                        let next_upstream_response = next_upstream_response.clone();
                        async move {
                            let path = request.uri().path().to_string();
                            let headers = request.headers().clone();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
                            let expires_at =
                                (Utc::now() + TimeDelta::minutes(5)).to_rfc3339();
                            let is_inference_response =
                                path == "/api/agent-auth/v1/responses";
                            let (status, response) = match path.as_str() {
                                "/api/agent-auth/grants" => (
                                    StatusCode::CREATED,
                                    json!({
                                        "token": "remote-account-1",
                                        "expiresAt": expires_at,
                                        "binding": {
                                            "ownerSubject": "owner-1",
                                            "logicalSessionId": "session-failover",
                                            "provider": "openai",
                                            "model": "gpt-5.6-sol",
                                            "harness": "codex",
                                            "routingMode": body["routingMode"],
                                            "requestedAccountId": body["requestedAccountId"],
                                            "source": "comet-local",
                                            "lifecycleEpoch": 1,
                                            "environment": "local",
                                            "backend": "oauth",
                                            "accountId": "account-1",
                                            "accountGeneration": 1,
                                        }
                                    }),
                                ),
                                "/api/agent-auth/grants/failure" => {
                                    captured.send(FailoverCapture {
                                        path,
                                        authorization: headers[AUTHORIZATION]
                                            .to_str()
                                            .unwrap()
                                            .to_string(),
                                        request_id: None,
                                        routing_mode: headers["x-agent-auth-routing-mode"]
                                            .to_str()
                                            .unwrap()
                                            .to_string(),
                                        requested_account_id: headers
                                            .get("x-agent-auth-requested-account-id")
                                            .and_then(|value| value.to_str().ok())
                                            .map(str::to_string),
                                        body,
                                    }).unwrap();
                                    (
                                        StatusCode::OK,
                                        json!({
                                            "retry": true,
                                            "grant": {
                                                "token": "remote-account-2",
                                                "expiresAt": expires_at,
                                                "binding": {
                                                    "ownerSubject": "owner-1",
                                                    "logicalSessionId": "session-failover",
                                                    "provider": "openai",
                                                    "model": "gpt-5.6-sol",
                                                    "harness": "codex",
                                                    "routingMode": "automatic",
                                                    "requestedAccountId": null,
                                                    "source": "comet-local",
                                                    "lifecycleEpoch": 1,
                                                    "environment": "local",
                                                    "backend": "oauth",
                                                    "accountId": "account-2",
                                                    "accountGeneration": 1,
                                                }
                                            }
                                        }),
                                    )
                                }
                                "/api/agent-auth/v1/responses" => {
                                    let authorization =
                                        headers[AUTHORIZATION].to_str().unwrap().to_string();
                                    captured.send(FailoverCapture {
                                        path,
                                        authorization: authorization.clone(),
                                        request_id: headers.get("x-agent-auth-request-id")
                                            .and_then(|value| value.to_str().ok())
                                            .map(str::to_string),
                                        routing_mode: headers["x-agent-auth-routing-mode"]
                                            .to_str()
                                            .unwrap()
                                            .to_string(),
                                        requested_account_id: headers
                                            .get("x-agent-auth-requested-account-id")
                                            .and_then(|value| value.to_str().ok())
                                            .map(str::to_string),
                                        body,
                                    }).unwrap();
                                    let response_index =
                                        next_upstream_response.fetch_add(1, Ordering::SeqCst);
                                    upstream_responses
                                        .get(response_index)
                                        .cloned()
                                        .unwrap_or_else(|| {
                                            (StatusCode::OK, json!({ "ok": true }))
                                        })
                                }
                                other => panic!("unexpected failover control-plane path {other}"),
                            };
                            let is_sse_failure = is_inference_response
                                && status == StatusCode::TOO_MANY_REQUESTS
                                && content_type.eq_ignore_ascii_case("text/event-stream");
                            let response_body = if is_sse_failure {
                                format!("event: error\ndata: {response}\n\n")
                            } else {
                                response.to_string()
                            };
                            let response_content_type = if is_sse_failure {
                                content_type
                            } else {
                                "application/json"
                            };
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(status)
                                    .header(CONTENT_TYPE, response_content_type)
                                    .body(Full::new(Bytes::from(response_body)))
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

    async fn transport_flaky_control_plane(
        captured: mpsc::UnboundedSender<FailoverCapture>,
    ) -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                let attempts = attempts.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let captured = captured.clone();
                        let attempts = attempts.clone();
                        async move {
                            let path = request.uri().path().to_string();
                            let headers = request.headers().clone();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
                            let expires_at =
                                (Utc::now() + TimeDelta::minutes(5)).to_rfc3339();
                            if path == "/api/agent-auth/grants" {
                                return Ok(Response::builder()
                                    .status(StatusCode::CREATED)
                                    .header(CONTENT_TYPE, "application/json")
                                    .body(Full::new(Bytes::from(
                                        json!({
                                            "token": "remote-account-1",
                                            "expiresAt": expires_at,
                                            "binding": {
                                                "ownerSubject": "owner-1",
                                                "logicalSessionId": "session-transport",
                                                "provider": "openai",
                                                "model": "gpt-5.6-sol",
                                                "harness": "codex",
                                                "routingMode": body["routingMode"],
                                                "requestedAccountId": body["requestedAccountId"],
                                                "source": "comet-local",
                                                "lifecycleEpoch": 1,
                                                "environment": "local",
                                                "backend": "oauth",
                                                "accountId": "account-1",
                                                "accountGeneration": 1,
                                            }
                                        })
                                        .to_string(),
                                    )))
                                    .unwrap());
                            }
                            if path == "/api/agent-auth/v1/responses" {
                                captured
                                    .send(FailoverCapture {
                                        path,
                                        authorization: headers[AUTHORIZATION]
                                            .to_str()
                                            .unwrap()
                                            .to_string(),
                                        request_id: headers
                                            .get("x-agent-auth-request-id")
                                            .and_then(|value| value.to_str().ok())
                                            .map(str::to_string),
                                        routing_mode: headers["x-agent-auth-routing-mode"]
                                            .to_str()
                                            .unwrap()
                                            .to_string(),
                                        requested_account_id: headers
                                            .get("x-agent-auth-requested-account-id")
                                            .and_then(|value| value.to_str().ok())
                                            .map(str::to_string),
                                        body,
                                    })
                                    .unwrap();
                                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                                    return Err(std::io::Error::other(
                                        "simulated connection close before response headers",
                                    ));
                                }
                                return Ok(Response::builder()
                                    .status(StatusCode::OK)
                                    .header(CONTENT_TYPE, "application/json")
                                    .body(Full::new(Bytes::from_static(b"{\"ok\":true}")))
                                    .unwrap());
                            }
                            panic!("unexpected transport control-plane path {path}");
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

    #[tokio::test]
    async fn retries_one_unstarted_transport_failure_on_the_same_account() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = transport_flaky_control_plane(captured_tx).await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare(
                "session-transport",
                HarnessId::Codex,
                Some("gpt-5.6-sol"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let payload = json!({ "model": "gpt-5.6-sol", "input": "retry transport" });
        let response = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .header("x-request-id", "transport-request")
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap(), json!({ "ok": true }));

        let first = captured_rx.recv().await.unwrap();
        let second = captured_rx.recv().await.unwrap();
        assert_eq!(first.path, "/api/agent-auth/v1/responses");
        assert_eq!(first.authorization, "Bearer remote-account-1");
        assert_eq!(second.authorization, first.authorization);
        assert_eq!(first.request_id.as_deref(), Some("transport-request"));
        assert_eq!(second.request_id, first.request_id);
        assert_eq!(second.body, first.body);
        assert_eq!(first.body, payload);
        assert!(captured_rx.try_recv().is_err());
    }

    async fn streaming_control_plane(
        captured: mpsc::UnboundedSender<CapturedRequest>,
        revoked: Arc<std::sync::atomic::AtomicBool>,
    ) -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                let revoked = revoked.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let captured = captured.clone();
                        let revoked = revoked.clone();
                        async move {
                            let path = request.uri().path().to_string();
                            let headers = request.headers().clone();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            if path == "/api/agent-auth/grants/revoke" {
                                revoked.store(true, std::sync::atomic::Ordering::SeqCst);
                                let body = futures::stream::once(async {
                                    Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"{\"ok\":true}")))
                                });
                                return Ok::<_, Infallible>(Response::new(BodyExt::boxed(StreamBody::new(body))));
                            }
                            if path == "/api/agent-auth/v1/responses" {
                                captured
                                    .send(CapturedRequest {
                                        authorization: headers[AUTHORIZATION].to_str().unwrap().to_string(),
                                        api_key: None,
                                        owner_subject: headers["x-agent-auth-owner-subject"].to_str().unwrap().to_string(),
                                        session_id: headers["x-agent-auth-session-id"].to_str().unwrap().to_string(),
                                        request_id: headers["x-agent-auth-request-id"].to_str().unwrap().to_string(),
                                        routing_mode: headers["x-agent-auth-routing-mode"].to_str().unwrap().to_string(),
                                        requested_account_id: None,
                                        body: serde_json::from_slice(&bytes).unwrap(),
                                    })
                                    .unwrap();
                                let body = futures::stream::once(async {
                                    Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"data: first\n\n")))
                                })
                                .chain(futures::stream::pending());
                                return Ok(Response::new(BodyExt::boxed(StreamBody::new(body))));
                            }
                            let expires_at = (Utc::now() + TimeDelta::minutes(5)).to_rfc3339();
                            let body = json!({
                                "token": "remote-agent-auth-grant",
                                "expiresAt": expires_at,
                                "binding": {
                                    "ownerSubject": "owner-1",
                                    "logicalSessionId": "session-1",
                                    "provider": "openai",
                                    "model": "gpt-5.6-sol",
                                    "harness": "codex",
                                    "routingMode": "automatic",
                                    "requestedAccountId": null,
                                    "source": "comet-local",
                                    "lifecycleEpoch": 1,
                                    "environment": "local",
                                    "backend": "oauth",
                                    "accountId": "account-1",
                                    "accountGeneration": 1
                                }
                            });
                            let body = futures::stream::once(async move {
                                Ok::<_, Infallible>(Frame::data(Bytes::from(body.to_string())))
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
    async fn issues_exact_grant_streams_through_loopback_and_revokes() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let origin = test_control_plane(captured_tx, revoked.clone()).await;
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

        let captured = captured_rx.recv().await.unwrap();
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-grant-1");
        assert_eq!(captured.owner_subject, "owner-1");
        assert_eq!(captured.session_id, "session-1");
        assert_eq!(captured.request_id, "request-1");
        assert_eq!(captured.routing_mode, "automatic");
        assert_eq!(captured.requested_account_id, None);
        assert_eq!(captured.body["input"], "hello");

        let mut expired_routes = relay.subscribe_expired_routes();
        relay.remove(&route.token).await;
        assert!(revoked.load(std::sync::atomic::Ordering::SeqCst));
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
    async fn projects_local_import_id_for_rebind_and_restores_it_on_expiration() {
        let local_id = "local-chat-b5b85d0f52a29e39da7656ab";
        let projected = agent_auth_logical_session_id(local_id).into_owned();
        let (rebind_tx, mut rebind_rx) = mpsc::unbounded_channel();
        let rebind_origin = rebind_control_plane(rebind_tx).await;
        let rebind_client =
            ScaffoldClient::new(rebind_origin, "project-1", Arc::new(StaticToken)).unwrap();
        InferenceRelay::start(rebind_client)
            .unwrap()
            .rebind(local_id)
            .await
            .unwrap();
        assert_eq!(
            rebind_rx.recv().await.unwrap(),
            json!({ "logicalSessionId": projected })
        );

        let (captured_tx, _captured_rx) = mpsc::unbounded_channel();
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let origin = test_control_plane(captured_tx, revoked.clone()).await;
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
        relay.remove(&route.token).await;
        assert!(revoked.load(std::sync::atomic::Ordering::SeqCst));
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
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let origin = test_control_plane(captured_tx, revoked.clone()).await;
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

        relay.remove(&original_token).await;
        assert!(revoked.load(std::sync::atomic::Ordering::SeqCst));

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
        assert_eq!(captured_rx.recv().await.unwrap().session_id, "persistent-session");

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
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let origin = test_control_plane(captured_tx, revoked).await;
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
        relay.remove(&anthropic.token).await;
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
    async fn retired_worker_credential_uses_the_post_revoke_parent_grant() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let origin = test_control_plane(captured_tx, revoked.clone()).await;
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
        relay.remove(&first.token).await;
        assert!(revoked.load(std::sync::atomic::Ordering::SeqCst));
        {
            let mut state = lock(&relay.inner.route_state);
            state.retired.insert(
                "persistent-worker-token".into(),
                RetiredRoute {
                    logical_session_id: "persistent-session".into(),
                    local_session_id: None,
                    owner_subject: "owner-1".into(),
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
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-grant-2");
        assert_eq!(captured.session_id, "persistent-session");
        assert!(matches!(
            expired_routes.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        relay.remove(&parent.token).await;
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
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let origin = streaming_control_plane(captured_tx, revoked.clone()).await;
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

        relay.remove(&route_to_remove.token).await;

        assert!(revoked.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(response.bytes().await.unwrap(), Bytes::from_static(b"data: first\n\n"));
    }

    #[tokio::test]
    async fn streams_a_generic_429_without_reporting_or_replaying() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let generic_failure = json!({ "error": { "code": "rate_limit_exceeded" } });
        let origin = failover_control_plane(
            captured_tx,
            vec![(StatusCode::TOO_MANY_REQUESTS, generic_failure.clone())],
        )
        .await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare(
                "session-failover",
                HarnessId::Codex,
                Some("gpt-5.6-sol"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let payload = json!({ "model": "gpt-5.6-sol", "input": "stay sticky" });
        let response = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.json::<Value>().await.unwrap(), generic_failure);

        let first = captured_rx.recv().await.unwrap();
        assert_eq!(first.authorization, "Bearer remote-account-1");
        assert!(captured_rx.try_recv().is_err());

        let later = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(later.status(), reqwest::StatusCode::OK);
        let later_capture = captured_rx.recv().await.unwrap();
        assert_eq!(later_capture.authorization, "Bearer remote-account-1");
    }

    #[tokio::test]
    async fn replays_confirmed_exhaustion_through_one_replacement_account() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = failover_control_plane(
            captured_tx,
            vec![(
                StatusCode::TOO_MANY_REQUESTS,
                json!({
                    "type": "error",
                    "error": {
                        "type": "rate_limit_error",
                        "message": "This request would exceed your account's rate limit."
                    }
                }),
            )],
        )
        .await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare(
                "session-failover",
                HarnessId::Codex,
                Some("gpt-5.6-sol"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let payload = json!({ "model": "gpt-5.6-sol", "input": "replay exactly once" });
        let response = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .header("x-request-id", "failover-request")
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap(), json!({ "ok": true }));

        let first = captured_rx.recv().await.unwrap();
        let report = captured_rx.recv().await.unwrap();
        let second = captured_rx.recv().await.unwrap();
        assert_eq!(first.path, "/api/agent-auth/v1/responses");
        assert_eq!(first.authorization, "Bearer remote-account-1");
        assert_eq!(first.request_id.as_deref(), Some("failover-request"));
        assert_eq!(first.body, payload);
        assert_eq!(report.path, "/api/agent-auth/grants/failure");
        assert_eq!(report.authorization, "Bearer remote-account-1");
        assert_eq!(report.body["failureClass"], "account_exhausted");
        assert_eq!(report.body["responseStarted"], false);
        assert_eq!(second.authorization, "Bearer remote-account-2");
        assert_eq!(second.request_id, first.request_id);
        assert_eq!(second.body, first.body);

        let later = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(later.status(), reqwest::StatusCode::OK);
        let later_capture = captured_rx.recv().await.unwrap();
        assert_eq!(later_capture.authorization, "Bearer remote-account-2");
    }

    #[tokio::test]
    async fn replays_usage_limit_reached_type_from_sse_error() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = failover_control_plane_with_content_type(
            captured_tx,
            vec![(
                StatusCode::TOO_MANY_REQUESTS,
                json!({
                    "error": {
                        "type": "usage_limit_reached",
                        "message": "The usage limit has been reached"
                    }
                }),
            )],
            "text/event-stream",
        )
        .await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare(
                "session-failover",
                HarnessId::Codex,
                Some("gpt-5.6-sol"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let payload = json!({ "model": "gpt-5.6-sol", "input": "rotate accounts" });
        let response = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap(), json!({ "ok": true }));

        let first = captured_rx.recv().await.unwrap();
        let report = captured_rx.recv().await.unwrap();
        let second = captured_rx.recv().await.unwrap();
        assert_eq!(first.authorization, "Bearer remote-account-1");
        assert_eq!(report.path, "/api/agent-auth/grants/failure");
        assert_eq!(report.body["failureClass"], "account_exhausted");
        assert_eq!(second.authorization, "Bearer remote-account-2");
    }

    #[tokio::test]
    async fn does_not_report_or_attempt_a_third_account_after_replay_fails() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let second_failure = json!({ "error": { "type": "usage_limit_error" } });
        let origin = failover_control_plane(
            captured_tx,
            vec![
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({ "error": { "code": "subscription_limit_reached" } }),
                ),
                (StatusCode::TOO_MANY_REQUESTS, second_failure.clone()),
            ],
        )
        .await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare(
                "session-failover",
                HarnessId::Codex,
                Some("gpt-5.6-sol"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let response = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .header("x-request-id", "bounded-replay-request")
            .json(&json!({ "model": "gpt-5.6-sol", "input": "stop after replay" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.json::<Value>().await.unwrap(), second_failure);

        let first = captured_rx.recv().await.unwrap();
        let report = captured_rx.recv().await.unwrap();
        let second = captured_rx.recv().await.unwrap();
        assert_eq!(first.authorization, "Bearer remote-account-1");
        assert_eq!(report.path, "/api/agent-auth/grants/failure");
        assert_eq!(report.body["failureClass"], "account_exhausted");
        assert_eq!(second.authorization, "Bearer remote-account-2");
        assert_eq!(second.request_id, first.request_id);
        assert!(captured_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn pinned_routes_never_report_failure_or_rotate_accounts() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = failover_control_plane(
            captured_tx,
            vec![(
                StatusCode::TOO_MANY_REQUESTS,
                json!({ "error": { "code": "usage_limit_reached" } }),
            )],
        )
        .await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare(
                "session-failover",
                HarnessId::Codex,
                Some("gpt-5.6-sol"),
                Some("account-1"),
            )
            .await
            .unwrap()
            .unwrap();
        let response = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .json(&json!({ "model": "gpt-5.6-sol", "input": "stay pinned" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        let first = captured_rx.recv().await.unwrap();
        assert_eq!(first.authorization, "Bearer remote-account-1");
        assert_eq!(first.routing_mode, "pinned");
        assert_eq!(first.requested_account_id.as_deref(), Some("account-1"));
        assert!(captured_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn replays_an_unstarted_401_through_the_next_sticky_account() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let origin = failover_control_plane(
            captured_tx,
            vec![(
                StatusCode::UNAUTHORIZED,
                json!({ "error": { "code": "unauthorized" } }),
            )],
        )
        .await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare(
                "session-failover",
                HarnessId::Codex,
                Some("gpt-5.6-sol"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let payload = json!({ "model": "gpt-5.6-sol", "input": "retry authentication" });
        let response = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let first = captured_rx.recv().await.unwrap();
        let report = captured_rx.recv().await.unwrap();
        let second = captured_rx.recv().await.unwrap();
        assert_eq!(first.authorization, "Bearer remote-account-1");
        assert_eq!(report.path, "/api/agent-auth/grants/failure");
        assert_eq!(report.body["failureClass"], "authentication_required");
        assert_eq!(report.body["responseStarted"], false);
        assert_eq!(second.authorization, "Bearer remote-account-2");
        assert_eq!(second.body, first.body);
    }

    #[tokio::test]
    async fn authenticates_anthropic_sdk_requests_without_forwarding_the_loopback_token() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let origin = test_control_plane(captured_tx, revoked).await;
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

        let response = reqwest::Client::new()
            .post(format!("{}/v1/messages", route.base_url))
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
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-grant-1");
        assert_eq!(captured.api_key, None);
        assert_eq!(captured.session_id, "session-anthropic");
        assert_eq!(captured.body["model"], "claude-opus-5");
    }

    #[test]
    fn rejects_nonlocal_and_nonexact_pinned_grants() {
        let mut request = AgentInferenceGrantRequest {
            logical_session_id: "session-1".into(),
            provider: "openai".into(),
            model: "gpt-5.6-sol".into(),
            harness: "codex".into(),
            routing_mode: AgentRoutingMode::Automatic,
            requested_account_id: None,
            lifecycle_epoch: 1,
        };
        let mut grant = AgentInferenceGrant {
            token: "remote-agent-auth-grant".into(),
            expires_at: (Utc::now() + TimeDelta::minutes(5)).to_rfc3339(),
            binding: AgentInferenceGrantBinding {
                owner_subject: "owner-1".into(),
                logical_session_id: request.logical_session_id.clone(),
                provider: request.provider.clone(),
                model: request.model.clone(),
                harness: request.harness.clone(),
                routing_mode: request.routing_mode,
                requested_account_id: request.requested_account_id.clone(),
                source: "comet-local".into(),
                lifecycle_epoch: request.lifecycle_epoch,
                environment: "staging".into(),
                backend: "oauth".into(),
                account_id: Some("account-1".into()),
                account_generation: Some(1),
            },
        };

        assert!(validate_grant(&grant, &request).is_err());

        request.routing_mode = AgentRoutingMode::Pinned;
        request.requested_account_id = Some("account-1".into());
        grant.binding.environment = "local".into();
        grant.binding.routing_mode = AgentRoutingMode::Pinned;
        grant.binding.requested_account_id = Some("account-1".into());
        grant.binding.backend = "bifrost".into();
        grant.binding.account_id = None;
        grant.binding.account_generation = None;
        assert!(validate_grant(&grant, &request).is_err());

        grant.binding.backend = "oauth".into();
        grant.binding.account_id = Some("account-2".into());
        grant.binding.account_generation = Some(1);
        assert!(validate_grant(&grant, &request).is_err());
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
    fn grant_requests_serialize_automatic_and_pinned_routing_intent() {
        let automatic =
            inference_grant_request("session-1", HarnessId::Codex, Some("gpt-5.6-sol"), None, 1)
                .unwrap()
                .unwrap();
        assert_eq!(
            serde_json::to_value(&automatic).unwrap(),
            json!({
                "logicalSessionId": "session-1",
                "provider": "openai",
                "model": "gpt-5.6-sol",
                "harness": "codex",
                "routingMode": "automatic",
                "lifecycleEpoch": 1,
            })
        );

        let pinned = inference_grant_request(
            "session-1",
            HarnessId::ClaudeCode,
            Some("claude-opus-5"),
            Some("opaque-account-id"),
            2,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::to_value(&pinned).unwrap(),
            json!({
                "logicalSessionId": "session-1",
                "provider": "anthropic",
                "model": "claude-opus-5",
                "harness": "claude-code",
                "routingMode": "pinned",
                "requestedAccountId": "opaque-account-id",
                "lifecycleEpoch": 2,
            })
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
            Some(("openai", "gpt-5.6-sol".into()))
        );
        assert_eq!(inference_binding(HarnessId::Omp, None), None);
        assert_eq!(
            inference_binding(HarnessId::PrimeAgent, Some("default")),
            None
        );
    }
}
