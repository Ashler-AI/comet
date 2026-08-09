#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use serde_json::Value;
    use tokio::sync::mpsc;

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
                                        "token": "remote-agent-auth-grant",
                                        "expiresAt": expires_at,
                                        "binding": {
                                            "ownerSubject": "owner-1",
                                            "logicalSessionId": input["logicalSessionId"],
                                            "provider": input["provider"],
                                            "model": input["model"],
                                            "harness": input["harness"],
                                            "source": "comet-local",
                                            "lifecycleEpoch": input["lifecycleEpoch"],
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

    #[tokio::test]
    async fn issues_exact_grant_streams_through_loopback_and_revokes() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let origin = test_control_plane(captured_tx, revoked.clone()).await;
        let client = ScaffoldClient::new(origin, "project-1", Arc::new(StaticToken)).unwrap();
        let relay = InferenceRelay::start(client).unwrap();
        let route = relay
            .prepare("session-1", HarnessId::Codex, Some("gpt-5.6-sol"))
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
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-grant");
        assert_eq!(captured.owner_subject, "owner-1");
        assert_eq!(captured.session_id, "session-1");
        assert_eq!(captured.request_id, "request-1");
        assert_eq!(captured.body["input"], "hello");

        relay.remove(&route.token).await;
        assert!(revoked.load(std::sync::atomic::Ordering::SeqCst));
        let removed = http
            .get(format!("{}/v1/models", route.base_url))
            .bearer_auth(&route.token)
            .send()
            .await
            .unwrap();
        assert_eq!(removed.status(), reqwest::StatusCode::UNAUTHORIZED);
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
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-grant");
        assert_eq!(captured.api_key, None);
        assert_eq!(captured.session_id, "session-anthropic");
        assert_eq!(captured.body["model"], "claude-opus-5");
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
    fn normalizes_harness_provider_selectors_without_losing_paid_models() {
        assert_eq!(
            inference_binding(HarnessId::Omp, Some("openai-codex/gpt-5.6-sol")),
            Some(("openai", "gpt-5.6-sol".into()))
        );
        assert_eq!(
            inference_binding(HarnessId::PrimeAgent, Some("anthropic/claude-opus-5")),
            Some(("anthropic", "claude-opus-5".into()))
        );
        assert_eq!(
            inference_binding(
                HarnessId::PrimeAgent,
                Some("prime-inference/x-ai/grok-4.20")
            ),
            Some(("openai", "prime-inference/x-ai/grok-4.20".into()))
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
    }
}
