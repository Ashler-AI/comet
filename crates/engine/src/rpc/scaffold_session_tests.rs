use super::*;
use std::cell::Cell;

fn params() -> PrepareScaffoldSessionParams {
    PrepareScaffoldSessionParams {
        scope: CollaborationScope {
            project_id: "project-a".into(),
            deployment_id: Some("deployment-a".into()),
            session_id: Some("00000000-0000-4000-8000-000000000001".into()),
            unknown: Default::default(),
        },
        name: Some("Native handoff".into()),
        source_ref: Some("master".into()),
        database_environment: ScaffoldDatabaseEnvironment::Local,
        agent_route: AgentRoute::automatic(comet_proto::AgentProvider::OpenAi, "gpt-6-astra"),
        omp_handoff: Some(OmpHandoff {
            native_session_id: "native-source".into(),
            cwd: "/source".into(),
        }),
    }
}

fn response(
    scope: &CollaborationScope,
    lifecycle: ScaffoldLifecycle,
) -> ScaffoldEnvironmentControlResult {
    ScaffoldEnvironmentControlResult {
        preparation_generation: None,
        environment: comet_proto::SessionEnvironment {
            source: SessionEnvironmentSource::Scaffold {
                sandbox_id: "sandbox-a".into(),
                region: None,
                lifecycle,
                lifecycle_epoch: Some(1),
                links: Default::default(),
            },
            name: None,
            owner_principal: "owner@example.com".into(),
            scope: scope.clone(),
            source_ref: None,
            last_activity_at: None,
            database_environment: Some(ScaffoldDatabaseEnvironment::Local),
            unknown: Default::default(),
        },
        attached_device_id: Some("comet-scaffold-sandbox-a-e1".into()),
        run_id: None,
        room_projection: Some(SessionRoomProjection {
            project_id: scope.project_id.clone(),
            deployment_id: scope.deployment_id.clone().unwrap(),
            session_id: scope.session_id.clone().unwrap(),
        }),
        control_grant: Some(comet_proto::ScaffoldControlGrant {
            id: "grant-a".into(),
            expires_at: crate::now_ms() + 60_000,
            capabilities: vec![comet_proto::CAPABILITY_SESSION_CHAT.into()],
        }),
        handoff_native_session_id: None,
        handoff_cwd: None,
    }
}

#[tokio::test]
async fn native_handoff_bootstraps_before_readiness_and_never_recreates_on_attach_retry() {
    let parameters = params();
    let scope = parameters.scope.clone();
    let created = Cell::new(false);
    let attached = Cell::new(false);
    let attempts = Cell::new(0);
    let result = prepare_scaffold_session_with(parameters, None, |operation| {
        let result = match operation {
            ScaffoldEnvironmentControl::Create { .. } => {
                assert!(
                    !created.replace(true),
                    "a retry must not allocate a second sandbox"
                );
                Ok(response(&scope, ScaffoldLifecycle::Starting))
            }
            ScaffoldEnvironmentControl::Attach { .. } => {
                assert!(created.get());
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(RpcError::Failed(
                        "scaffold_api_error:503:sandbox_runtime_starting".into(),
                    ))
                } else {
                    attached.set(true);
                    Ok(response(&scope, ScaffoldLifecycle::Starting))
                }
            }
            ScaffoldEnvironmentControl::Inspect { .. } => {
                assert!(
                    attached.get(),
                    "readiness requires the bootstrapped remote host"
                );
                Ok(response(&scope, ScaffoldLifecycle::Ready))
            }
            ScaffoldEnvironmentControl::HandoffOmpSession { .. } => {
                let mut value = response(&scope, ScaffoldLifecycle::Ready);
                value.handoff_native_session_id = Some("native-source".into());
                value.handoff_cwd = Some("/workspace/ashler-platform".into());
                Ok(value)
            }
            _ => panic!("unexpected lifecycle action"),
        };
        std::future::ready(result)
    })
    .await
    .unwrap();
    assert_eq!(
        result.handoff_native_session_id.as_deref(),
        Some("native-source")
    );
    assert_eq!(
        result.handoff_cwd.as_deref(),
        Some("/workspace/ashler-platform")
    );
    assert!(matches!(
        result.environment.source,
        SessionEnvironmentSource::Scaffold {
            lifecycle: ScaffoldLifecycle::Ready,
            ..
        }
    ));
}

#[tokio::test]
async fn mismatched_attachment_never_transfers_source_history() {
    let parameters = params();
    let scope = parameters.scope.clone();
    let error = prepare_scaffold_session_with(parameters, None, |operation| {
        let mut value = response(&scope, ScaffoldLifecycle::Starting);
        match operation {
            ScaffoldEnvironmentControl::Create { .. } => {}
            ScaffoldEnvironmentControl::Attach { .. } => {
                value.attached_device_id = Some("comet-scaffold-another-sandbox-e1".into());
            }
            _ => panic!("must fail closed before inspection or upload"),
        }
        std::future::ready(Ok(value))
    })
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("different device"));
    assert!(
        error.contains("sandbox-a"),
        "failed preparation must retain recovery identity"
    );
}

#[tokio::test]
async fn terminal_readiness_never_transfers_history_or_recreates() {
    for lifecycle in [
        ScaffoldLifecycle::Paused,
        ScaffoldLifecycle::Stopped,
        ScaffoldLifecycle::Failed,
    ] {
        let parameters = params();
        let scope = parameters.scope.clone();
        let inspected = Cell::new(false);
        let result = prepare_scaffold_session_with(parameters, None, |operation| {
            let value = match operation {
                ScaffoldEnvironmentControl::Create { .. }
                | ScaffoldEnvironmentControl::Attach { .. } => {
                    assert!(!inspected.get(), "terminal targets must not be retried");
                    response(&scope, ScaffoldLifecycle::Starting)
                }
                ScaffoldEnvironmentControl::Inspect { .. } => {
                    assert!(
                        !inspected.replace(true),
                        "terminal readiness must stop immediately"
                    );
                    response(&scope, lifecycle)
                }
                _ => panic!("terminal targets must not receive source history"),
            };
            std::future::ready(Ok(value))
        })
        .await;
        assert!(result.is_err());
        assert!(inspected.get());
    }
}

#[tokio::test]
async fn lifecycle_epoch_changes_invalidate_prepared_authority() {
    for changes_during_transfer in [false, true] {
        let parameters = params();
        let scope = parameters.scope.clone();
        let transferred = Cell::new(false);
        let result = prepare_scaffold_session_with(parameters, None, |operation| {
            let mut value = response(&scope, ScaffoldLifecycle::Ready);
            let change_epoch = match operation {
                ScaffoldEnvironmentControl::Create { .. }
                | ScaffoldEnvironmentControl::Attach { .. } => false,
                ScaffoldEnvironmentControl::Inspect { .. } => !changes_during_transfer,
                ScaffoldEnvironmentControl::HandoffOmpSession { .. } => {
                    assert!(changes_during_transfer, "stale authority must block upload");
                    transferred.set(true);
                    value.handoff_native_session_id = Some("native-source".into());
                    value.handoff_cwd = Some("/workspace/ashler-platform".into());
                    true
                }
                _ => panic!("unexpected lifecycle action"),
            };
            if change_epoch {
                let SessionEnvironmentSource::Scaffold {
                    lifecycle_epoch, ..
                } = &mut value.environment.source
                else {
                    unreachable!()
                };
                *lifecycle_epoch = Some(2);
            }
            std::future::ready(Ok(value))
        })
        .await;
        assert!(
            result.is_err(),
            "stale device authority must never be admitted"
        );
        assert_eq!(transferred.get(), changes_during_transfer);
    }
}

#[tokio::test]
async fn lost_creation_response_is_not_retried() {
    let calls = Cell::new(0);
    let result = prepare_scaffold_session_with(params(), None, |_| {
        calls.set(calls.get() + 1);
        std::future::ready(Err(RpcError::Failed(
            "scaffold_api_error:503:scaffold_request_rejected".into(),
        )))
    })
    .await;
    assert!(result.is_err());
    assert_eq!(calls.get(), 1);
}

#[tokio::test]
async fn accepted_preparation_recovers_without_allocating_another_sandbox() {
    let parameters = params();
    let scope = parameters.scope.clone();
    let mut accepted = response(&scope, ScaffoldLifecycle::Starting).environment;
    let SessionEnvironmentSource::Scaffold {
        lifecycle_epoch, ..
    } = &mut accepted.source
    else {
        unreachable!()
    };
    *lifecycle_epoch = None;
    let result = prepare_scaffold_session_with(parameters, Some(accepted), |operation| {
        let mut result = response(&scope, ScaffoldLifecycle::Ready);
        match operation {
            ScaffoldEnvironmentControl::Attach { sandbox_id, .. }
            | ScaffoldEnvironmentControl::Inspect { sandbox_id, .. } => {
                assert_eq!(sandbox_id, "sandbox-a")
            }
            ScaffoldEnvironmentControl::HandoffOmpSession { sandbox_id, .. } => {
                assert_eq!(sandbox_id, "sandbox-a");
                result.handoff_native_session_id = Some("native-source".into());
                result.handoff_cwd = Some("/workspace/ashler-platform".into());
            }
            _ => panic!("recovery must not allocate another sandbox"),
        }
        std::future::ready(Ok(result))
    })
    .await
    .unwrap();
    assert_eq!(
        result.handoff_native_session_id.as_deref(),
        Some("native-source")
    );
}

#[tokio::test(start_paused = true)]
async fn healthy_preparation_outlives_the_old_deadline_and_resets_transient_faults() {
    let mut parameters = params();
    parameters.omp_handoff = None;
    let scope = parameters.scope.clone();
    let created = Cell::new(false);
    let inspections = Cell::new(0);
    let started = tokio::time::Instant::now();
    let result = prepare_scaffold_session_with(parameters, None, |operation| {
        let scope = &scope;
        let created = &created;
        let inspections = &inspections;
        async move {
            match operation {
                ScaffoldEnvironmentControl::Create { .. } => {
                    assert!(!created.replace(true), "startup must never recreate");
                    Ok(response(scope, ScaffoldLifecycle::Starting))
                }
                ScaffoldEnvironmentControl::Attach { .. } => {
                    Ok(response(scope, ScaffoldLifecycle::Starting))
                }
                ScaffoldEnvironmentControl::Inspect { .. } => {
                    let attempt = inspections.get();
                    inspections.set(attempt + 1);
                    // All requests stay below the unchanged HTTP timeout. Valid
                    // Starting between transient failures resets only fault time.
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    if attempt == 64 {
                        Ok(response(scope, ScaffoldLifecycle::Ready))
                    } else if attempt % 2 == 0 {
                        Err(RpcError::Failed(
                            "scaffold_api_error:503:scaffold_request_rejected".into(),
                        ))
                    } else {
                        Ok(response(scope, ScaffoldLifecycle::Starting))
                    }
                }
                _ => panic!("startup must not change lifecycle or send a first command"),
            }
        }
    })
    .await
    .unwrap();
    assert!(started.elapsed() > Duration::from_secs(10 * 60));
    assert!(matches!(result.environment.source, SessionEnvironmentSource::Scaffold {
        lifecycle: ScaffoldLifecycle::Ready, ..
    }));
}

#[tokio::test(start_paused = true)]
async fn repeated_transient_faults_end_without_recreation_or_lifecycle_failure() {
    for failing_attach in [true, false] {
        let parameters = params();
        let scope = parameters.scope.clone();
        let created = Cell::new(false);
        let started = tokio::time::Instant::now();
        let error = prepare_scaffold_session_with(parameters, None, |operation| {
            let result = match operation {
                ScaffoldEnvironmentControl::Create { .. } => {
                    assert!(!created.replace(true));
                    Ok(response(&scope, ScaffoldLifecycle::Starting))
                }
                ScaffoldEnvironmentControl::Attach { .. } if !failing_attach => {
                    Ok(response(&scope, ScaffoldLifecycle::Starting))
                }
                ScaffoldEnvironmentControl::Attach { .. }
                | ScaffoldEnvironmentControl::Inspect { .. } => Err(RpcError::Failed(
                    "scaffold_api_error:503:scaffold_request_rejected".into(),
                )),
                _ => panic!("faults must not transfer, recreate, or change lifecycle"),
            };
            std::future::ready(result)
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(started.elapsed() >= Duration::from_secs(10 * 60));
        assert!(started.elapsed() < Duration::from_secs(10 * 60 + 1));
        assert!(error.contains("scaffold_api_error:503:scaffold_request_rejected"));
        assert!(error.contains("sandbox-a"));
        assert!(!error.contains("terminal lifecycle"));
    }
}

#[tokio::test]
async fn preparation_returns_nonretryable_errors_and_cancellation_without_recreation() {
    for failure in [
        "scaffold_auth_unavailable",
        "scaffold_request_failed: connection closed",
        "scaffold_response_invalid: unknown lifecycle variant",
        "scaffold_request_cancelled",
    ] {
        for failing_attach in [true, false] {
            let parameters = params();
            let scope = parameters.scope.clone();
            let failed = Cell::new(false);
            let created = Cell::new(false);
            let error = prepare_scaffold_session_with(parameters, None, |operation| {
                assert!(!failed.get(), "nonretryable errors must end the wait");
                let result = match operation {
                    ScaffoldEnvironmentControl::Create { .. } => {
                        assert!(!created.replace(true));
                        Ok(response(&scope, ScaffoldLifecycle::Starting))
                    }
                    ScaffoldEnvironmentControl::Attach { .. } if !failing_attach => {
                        Ok(response(&scope, ScaffoldLifecycle::Starting))
                    }
                    ScaffoldEnvironmentControl::Attach { .. }
                    | ScaffoldEnvironmentControl::Inspect { .. } => {
                        failed.set(true);
                        Err(RpcError::Failed(failure.into()))
                    }
                    _ => panic!("failed preparation must not send commands or change lifecycle"),
                };
                std::future::ready(result)
            })
            .await
            .unwrap_err()
            .to_string();
            assert!(error.contains(failure));
            assert!(error.contains("sandbox-a"));
            assert!(!error.contains("terminal lifecycle"));
        }
    }
}

#[tokio::test]
async fn nonlocal_and_non_omp_sources_are_rejected_before_sandbox_creation() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let core = crate::EngineCore::assemble_with_identity(
        dir.path(),
        std::sync::Arc::new(crate::HarnessRegistry::new()),
        HarnessId::Omp,
        None,
        "project-a",
        "owner@example.com",
        RuntimeProfile::LocalController,
    )
    .unwrap();
    let mut auth_config = crate::AuthConfig::new(&origin, dir.path());
    auth_config.project_scope = "project-a".into();
    auth_config.dev_user_id = "owner@example.com".into();
    core.set_auth(crate::Auth::new(auth_config));
    core.set_scaffold_runtime(crate::ScaffoldRuntime::new(
        crate::ScaffoldClient::new(
            &origin,
            "project-a",
            std::sync::Arc::new(comet_rpc::StaticToken("test".into())),
        )
        .unwrap(),
        &origin,
        std::sync::Arc::new(crate::UnavailableDeviceJoinGrantProvider),
    ));
    core.workspace
        .create_space("source-space", &core.device_id, "/source", None, false)
        .unwrap();
    let source_id = "00000000-0000-4000-8000-000000000001";
    core.workspace
        .create_chat(
            source_id,
            "source-space",
            Some(ChatConfig {
                harness: HarnessId::Codex,
                model: Some("openai-codex/gpt-6-astra".into()),
                reasoning: None,
                agent_account_id: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
            }),
            None,
        )
        .unwrap();
    let rpc = core.rpc_service();
    let request = || HandoffSessionToScaffoldParams {
        source_chat_id: source_id.into(),
        prompt: "Continue remotely".into(),
        database_environment: ScaffoldDatabaseEnvironment::Local,
    };
    assert!(
        rpc.handoff_session_to_scaffold(request())
            .await
            .unwrap_err()
            .to_string()
            .contains("requires_omp")
    );
    core.workspace
        .set_chat_host(source_id, "different-device")
        .unwrap();
    assert!(
        rpc.handoff_session_to_scaffold(request())
            .await
            .unwrap_err()
            .to_string()
            .contains("not_hosted_here")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), listener.accept())
            .await
            .is_err()
    );
    core.shutdown().await;
}

#[tokio::test]
async fn concurrent_and_interrupted_preparation_never_duplicates_creation() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let core = crate::EngineCore::assemble_with_identity(
        dir.path(),
        std::sync::Arc::new(crate::HarnessRegistry::new()),
        HarnessId::Omp,
        None,
        "project-a",
        "owner@example.com",
        RuntimeProfile::LocalController,
    )
    .unwrap();
    let mut auth_config = crate::AuthConfig::new(&origin, dir.path());
    auth_config.project_scope = "project-a".into();
    auth_config.dev_user_id = "owner@example.com".into();
    core.set_auth(crate::Auth::new(auth_config));
    core.set_scaffold_runtime(
        crate::ScaffoldRuntime::new(
            crate::ScaffoldClient::new(
                &origin,
                "project-a",
                std::sync::Arc::new(comet_rpc::StaticToken("test".into())),
            )
            .unwrap(),
            &origin,
            std::sync::Arc::new(crate::UnavailableDeviceJoinGrantProvider),
        )
        .with_deployment_id("deployment-a".into()),
    );
    let first_rpc = core.rpc_service();
    let first = tokio::spawn(async move { first_rpc.prepare_scaffold_session(params()).await });
    let (connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    // The first request is waiting for its create response. A separate RPC
    // service must reject the same scope without sending another request.
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        core.rpc_service().prepare_scaffold_session(params()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("already in progress"));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), listener.accept())
            .await
            .is_err()
    );
    // Accept creation without an epoch, then interrupt while Attach is waiting
    // for the owner room. The durable ref must precede that wait and any upload.
    let mut reader = BufReader::new(connection);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(line, "POST /api/code-sandboxes HTTP/1.1\r\n");
    let mut content_length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = length.trim().parse::<usize>().unwrap();
        }
    }
    reader
        .read_exact(&mut vec![0; content_length])
        .await
        .unwrap();
    let body = serde_json::json!({"sandbox": {
        "id": "sandbox-a", "status": "creating", "runtimeProfile": "remote_code",
        "ownerEmail": "owner@example.com", "createdAt": "2026-08-04T00:00:00Z",
        "updatedAt": "2026-08-04T00:00:00Z"
    }})
    .to_string();
    reader.get_mut().write_all(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    ).as_bytes()).await.unwrap();
    let scope = params().scope;
    let accepted = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(environment) = core
                .workspace
                .doc()
                .session_ref("owner@example.com", scope.session_id.as_deref().unwrap())
                .unwrap()
                .and_then(|reference| reference.environment)
            {
                break environment;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(accepted.scope, scope);
    assert!(
        matches!(accepted.source, SessionEnvironmentSource::Scaffold {
        ref sandbox_id, lifecycle_epoch: None, ..
    } if sandbox_id == "sandbox-a")
    );
    first.abort();
    let _ = first.await;
    let retry_rpc = core.rpc_service();
    tokio::select! {
        result = retry_rpc.prepare_scaffold_session(params()) => {
            panic!("recovery should await the same target's owner room: {result:?}");
        }
        _ = listener.accept() => panic!("retry must not allocate another sandbox"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    core.shutdown().await;
}

#[tokio::test]
async fn preparation_creates_when_saved_environment_is_local_or_out_of_scope() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let scope = params().scope;
    let mut local = response(&scope, ScaffoldLifecycle::Ready).environment;
    local.source = SessionEnvironmentSource::Local;
    let mut other_deployment = response(&scope, ScaffoldLifecycle::Ready).environment;
    other_deployment.scope.deployment_id = Some("another-deployment".into());
    for environment in [local, other_deployment] {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let core = crate::EngineCore::assemble_with_identity(
            dir.path(),
            std::sync::Arc::new(crate::HarnessRegistry::new()),
            HarnessId::Omp,
            None,
            "project-a",
            "owner@example.com",
            RuntimeProfile::LocalController,
        )
        .unwrap();
        let mut auth_config = crate::AuthConfig::new(&origin, dir.path());
        auth_config.project_scope = "project-a".into();
        auth_config.dev_user_id = "owner@example.com".into();
        core.set_auth(crate::Auth::new(auth_config));
        core.set_scaffold_runtime(
            crate::ScaffoldRuntime::new(
                crate::ScaffoldClient::new(
                    &origin,
                    "project-a",
                    std::sync::Arc::new(comet_rpc::StaticToken("test".into())),
                )
                .unwrap(),
                &origin,
                std::sync::Arc::new(crate::UnavailableDeviceJoinGrantProvider),
            )
            .with_deployment_id("deployment-a".into()),
        );
        core.workspace
            .upsert_session_ref(scope.session_id.as_deref().unwrap(), Some(environment))
            .unwrap();
        let request = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut first_line = String::new();
            reader.read_line(&mut first_line).await.unwrap();
            assert_eq!(first_line, "POST /api/code-sandboxes HTTP/1.1\r\n");
            // Refuse creation at the provider boundary; the assertion above
            // proves neither a local reference nor another deployment was reused.
            reader.get_mut().write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").await.unwrap();
        };
        let rpc = core.rpc_service();
        tokio::time::timeout(Duration::from_secs(2), async {
            let (result, ()) = tokio::join!(rpc.prepare_scaffold_session(params()), request);
            assert!(result.is_err());
        })
        .await
        .unwrap();
        core.shutdown().await;
    }
}

#[tokio::test]
async fn prepared_handoff_persists_a_distinct_chat_and_remote_resume_command() {
    let dir = tempfile::tempdir().unwrap();
    let core = crate::EngineCore::assemble_with_identity(
        dir.path(),
        std::sync::Arc::new(crate::HarnessRegistry::new()),
        HarnessId::Omp,
        None,
        "project-a",
        "owner@example.com",
        RuntimeProfile::LocalController,
    )
    .unwrap();
    let mut auth_config = crate::AuthConfig::new("http://127.0.0.1:1", dir.path());
    auth_config.project_scope = "project-a".into();
    auth_config.dev_user_id = "owner@example.com".into();
    core.set_auth(crate::Auth::new(auth_config));
    core.workspace
        .create_space("source-space", &core.device_id, "/source", None, false)
        .unwrap();
    let source_id = "00000000-0000-4000-8000-000000000002";
    core.workspace
        .create_chat(
            source_id,
            "source-space",
            Some(ChatConfig {
                harness: HarnessId::Omp,
                model: Some("openai-codex/gpt-6-astra".into()),
                reasoning: None,
                agent_account_id: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
            }),
            None,
        )
        .unwrap();
    let source = core.workspace.doc().chat(source_id).unwrap().unwrap();
    let mut prepared = response(&params().scope, ScaffoldLifecycle::Ready);
    prepared.handoff_native_session_id = Some("native-source".into());
    prepared.handoff_cwd = Some("/workspace/ashler-platform".into());
    let target_id = prepared.environment.scope.session_id.clone().unwrap();
    core.doc_host
        .open_projection(&target_id, prepared.room_projection.as_ref())
        .unwrap();
    core.workspace
        .upsert_session_ref(&target_id, Some(prepared.environment.clone()))
        .unwrap();
    let mut startup = PreparationOutcome::begin(core.workspace.clone(), &target_id).unwrap();
    startup.armed = false;
    prepared.preparation_generation = Some(startup.startup.generation.clone());
    let mut stale = prepared.clone();
    stale.preparation_generation = Some("older-preparation".into());
    assert!(core.rpc_service().admit_scaffold_handoff(
        source.clone(), "Stale task".into(), "openai-codex/gpt-6-astra".into(),
        "owner@example.com".into(), stale,
    ).is_err());
    assert!(core.workspace.doc().chat(&target_id).unwrap().is_none());
    assert!(!core.doc_host.chat_has_commands(&target_id).unwrap());

    let receipt = core
        .rpc_service()
        .admit_scaffold_handoff(
            source.clone(),
            "Continue the exact task".into(),
            "openai-codex/gpt-6-astra".into(),
            "owner@example.com".into(),
            prepared,
        )
        .unwrap();
    assert_ne!(receipt.chat_id, source_id);
    assert_eq!(core.workspace.doc().chat(source_id).unwrap(), Some(source));
    let remote = core
        .workspace
        .doc()
        .chat(&receipt.chat_id)
        .unwrap()
        .unwrap();
    assert_eq!(remote.space_id.as_deref(), Some("source-space"));
    assert_eq!(remote.harness_session_id.as_deref(), Some("native-source"));
    let command = core
        .doc_host
        .command_entry(&receipt.chat_id, &receipt.command_id)
        .unwrap()
        .unwrap();
    let SessionCommandPayload::Control {
        source,
        owner_device_id,
        action,
        ..
    } = command.payload
    else {
        panic!("handoff must never enqueue a local run")
    };
    assert_eq!(source, AgentSessionSource::Scaffold);
    assert_eq!(owner_device_id, "comet-scaffold-sandbox-a-e1");
    let comet_doc::SessionControlAction::Start { request, .. } = *action else {
        panic!("expected remote start")
    };
    assert_eq!(request.resume.as_deref(), Some("native-source"));
    assert_eq!(request.cwd, "/workspace/ashler-platform");
    assert_eq!(request.prompt, "Continue the exact task");
    let admitted = core.workspace.session_startup(&receipt.chat_id).unwrap().unwrap();
    assert_eq!(admitted.generation, startup.startup.generation);
    assert_eq!(admitted.status, comet_proto::SessionStartupStatus::Admitted);
    assert_eq!(admitted.command_id.as_deref(), Some(receipt.command_id.as_str()));
    core.shutdown().await;
}

#[tokio::test]
async fn initial_queue_requires_exact_preparation_but_admitted_followups_do_not() {
    use comet_proto::{SessionStartup, SessionStartupStatus};
    let dir = tempfile::tempdir().unwrap();
    let core = crate::EngineCore::assemble_with_identity(
        dir.path(), std::sync::Arc::new(crate::HarnessRegistry::new()), HarnessId::Omp,
        None, "project-a", "owner@example.com", RuntimeProfile::LocalController,
    ).unwrap();
    let chat_id = params().scope.session_id.unwrap();
    core.workspace.create_space("space", &core.device_id, "/workspace", None, false).unwrap();
    core.workspace.create_chat(&chat_id, "space", None, None).unwrap();
    let preparing = SessionStartup {
        generation: "new-attempt".into(), status: SessionStartupStatus::Preparing,
        updated_at: chrono::Utc::now(), command_id: None,
    };
    core.workspace.update_session_startup(&chat_id, None, preparing.clone()).unwrap();
    let request = RunRequest {
        prompt: "Start once".into(), model: None, agent_account_id: None, reasoning: None,
        model_options: Default::default(), cwd: "/workspace".into(),
        sandbox: SandboxLevel::WorkspaceWrite, auto_approve: true, resume: None,
        attachments: Vec::new(),
    };
    let run = SessionCommandPayload::Run { request: request.clone(), message_id: "message".into() };
    let control = SessionCommandPayload::Control {
        session_id: chat_id.clone(), owner_device_id: "remote-owner".into(),
        actor_device_id: core.device_id.clone(), actor_subject: "owner@example.com".into(),
        grant_id: "grant".into(), source: AgentSessionSource::Scaffold,
        action: Box::new(comet_doc::SessionControlAction::Start {
            request, message_id: "remote-message".into(),
        }),
    };
    let rpc = core.rpc_service();
    for command in [&run, &control] {
        for generation in [None, Some("old-attempt")] {
            assert!(rpc.handle(methods::QUEUE_COMMAND, serde_json::json!({
                "chatId": chat_id, "commandId": "rejected", "command": command,
                "preparationGeneration": generation,
            })).await.is_err());
            assert!(!core.doc_host.chat_has_commands(&chat_id).unwrap());
            assert_eq!(core.workspace.session_startup(&chat_id).unwrap(), Some(preparing.clone()));
        }
    }
    rpc.handle(methods::QUEUE_COMMAND, serde_json::json!({
        "chatId": chat_id, "commandId": "first", "command": run,
        "preparationGeneration": "new-attempt",
    })).await.unwrap();
    assert!(core.doc_host.command_entry(&chat_id, "first").unwrap().is_some());
    let admitted = core.workspace.session_startup(&chat_id).unwrap().unwrap();
    assert_eq!(admitted.status, SessionStartupStatus::Admitted);
    assert_eq!(admitted.command_id.as_deref(), Some("first"));
    rpc.handle(methods::QUEUE_COMMAND, serde_json::json!({
        "chatId": chat_id, "commandId": "followup", "command": run,
    })).await.unwrap();
    assert!(core.doc_host.command_entry(&chat_id, "followup").unwrap().is_some());
    assert!(rpc.handle(methods::QUEUE_COMMAND, serde_json::json!({
        "chatId": chat_id, "commandId": "late-stale", "command": run,
        "preparationGeneration": "old-attempt",
    })).await.is_err());
    assert!(core.doc_host.command_entry(&chat_id, "late-stale").unwrap().is_none());
    assert_eq!(core.workspace.session_startup(&chat_id).unwrap(), Some(admitted));
    core.shutdown().await;
}

#[tokio::test]
async fn failure_reports_only_mark_the_matching_existing_unadmitted_preparation() {
    use comet_proto::{SessionStartup, SessionStartupStatus};
    let dir = tempfile::tempdir().unwrap();
    let core = crate::EngineCore::assemble_with_identity(
        dir.path(), std::sync::Arc::new(crate::HarnessRegistry::new()), HarnessId::Omp,
        None, "project-a", "owner@example.com", RuntimeProfile::LocalController,
    ).unwrap();
    let rpc = core.rpc_service();
    for (chat_id, status, reported_generation, changes) in [
        ("matched", SessionStartupStatus::Preparing, "current", true),
        ("stale", SessionStartupStatus::Preparing, "old", false),
        ("admitted", SessionStartupStatus::Admitted, "current", false),
        ("uncertain", SessionStartupStatus::CreationUncertain, "current", false),
    ] {
        let original = SessionStartup {
            generation: "current".into(), status, updated_at: chrono::Utc::now(),
            command_id: (status == SessionStartupStatus::Admitted).then(|| "first".into()),
        };
        core.workspace.update_session_startup(chat_id, None, original.clone()).unwrap();
        rpc.handle(methods::REPORT_SCAFFOLD_PREPARATION_FAILURE, serde_json::json!({
            "chatId": chat_id, "generation": reported_generation,
        })).await.unwrap();
        let actual = core.workspace.session_startup(chat_id).unwrap().unwrap();
        if changes {
            assert_eq!(actual.status, SessionStartupStatus::AttentionNeeded);
            assert_eq!(actual.generation, original.generation);
        } else {
            assert_eq!(actual, original);
        }
    }
    core.workspace.upsert_session_ref("untracked", None).unwrap();
    let untracked = core.workspace.doc().session_ref("owner@example.com", "untracked").unwrap();
    core.workspace.remove_session_ref("matched").unwrap();
    for chat_id in ["missing", "matched", "untracked"] {
        rpc.handle(methods::REPORT_SCAFFOLD_PREPARATION_FAILURE, serde_json::json!({
            "chatId": chat_id, "generation": "current",
        })).await.unwrap();
    }
    for chat_id in ["missing", "matched"] {
        assert!(core.workspace.doc().session_ref("owner@example.com", chat_id).unwrap().is_none());
        assert!(core.workspace.doc().chat(chat_id).unwrap().is_none());
    }
    assert_eq!(core.workspace.doc().session_ref("owner@example.com", "untracked").unwrap(), untracked);
    core.shutdown().await;
}

#[tokio::test]
async fn provider_auth_text_cannot_make_uncertain_creation_retryable() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use comet_proto::SessionStartupStatus;
    for pre_dispatch in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let core = crate::EngineCore::assemble_with_identity(
            dir.path(), std::sync::Arc::new(crate::HarnessRegistry::new()), HarnessId::Omp,
            None, "project-a", "owner@example.com", RuntimeProfile::LocalController,
        ).unwrap();
        let mut auth_config = crate::AuthConfig::new(&origin, dir.path());
        auth_config.project_scope = "project-a".into();
        auth_config.dev_user_id = "owner@example.com".into();
        core.set_auth(crate::Auth::new(auth_config));
        core.set_scaffold_runtime(crate::ScaffoldRuntime::new(
            crate::ScaffoldClient::new(
                &origin, "project-a",
                std::sync::Arc::new(comet_rpc::StaticToken(
                    if pre_dispatch { "" } else { "test" }.into(),
                )),
            ).unwrap(),
            &origin, std::sync::Arc::new(crate::UnavailableDeviceJoinGrantProvider),
        ).with_deployment_id("deployment-a".into()));
        let provider = async {
            let (connection, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(connection);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(line, "POST /api/code-sandboxes HTTP/1.1\r\n");
            let mut content_length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" { break; }
                if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = length.trim().parse::<usize>().unwrap();
                }
            }
            reader.read_exact(&mut vec![0; content_length]).await.unwrap();
            let body = r#"{"error":"scaffold_auth_unavailable","message":"scaffold_auth_unavailable"}"#;
            reader.get_mut().write_all(format!(
                "HTTP/1.1 500 Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len(),
            ).as_bytes()).await.unwrap();
        };
        let rpc = core.rpc_service();
        let error = if pre_dispatch {
            rpc.prepare_scaffold_session(params()).await.unwrap_err()
        } else {
            let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(rpc.prepare_scaffold_session(params()), provider)
            }).await.unwrap();
            result.unwrap_err()
        };
        assert!(error.to_string().contains("scaffold_auth_unavailable"));
        assert_eq!(matches!(error, RpcError::ScaffoldAuthUnavailable), pre_dispatch);
        let startup = core.workspace.session_startup(
            params().scope.session_id.as_deref().unwrap(),
        ).unwrap().unwrap();
        assert_eq!(startup.status, if pre_dispatch {
            SessionStartupStatus::AttentionNeeded
        } else {
            SessionStartupStatus::CreationUncertain
        });
        if !pre_dispatch {
            assert!(rpc.prepare_scaffold_session(params()).await.is_err());
        }
        assert!(tokio::time::timeout(Duration::from_millis(10), listener.accept()).await.is_err());
        core.shutdown().await;
    }
}
