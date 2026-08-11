#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::AgentInferenceGrantBinding;
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
                                        "token": "remote-agent-auth-grant",
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
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-grant");
        assert_eq!(captured.owner_subject, "owner-1");
        assert_eq!(captured.session_id, "session-1");
        assert_eq!(captured.request_id, "request-1");
        assert_eq!(captured.routing_mode, "automatic");
        assert_eq!(captured.requested_account_id, None);
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
        assert_eq!(captured.authorization, "Bearer remote-agent-auth-grant");
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
