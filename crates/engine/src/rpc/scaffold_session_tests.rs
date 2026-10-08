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
async fn explicit_scaffold_control_requires_existing_exact_deployment_grant() {
    let dir = tempfile::tempdir().unwrap();
    let core = crate::EngineCore::assemble_with_identity(
        dir.path(),
        std::sync::Arc::new(crate::default_registry(RuntimeProfile::Mock)),
        HarnessId::Mock,
        None,
        "project-a",
        "owner@example.com",
        RuntimeProfile::Mock,
    ).unwrap();
    let mut auth_config = crate::AuthConfig::new("http://127.0.0.1:1", dir.path());
    auth_config.project_scope = "project-a".into();
    auth_config.dev_user_id = "owner@example.com".into();
    core.set_auth(crate::Auth::new(auth_config));
    let prepared = response(&params().scope, ScaffoldLifecycle::Ready);
    let chat_id = prepared.environment.scope.session_id.clone().unwrap();
    core.doc_host.open_projection(&chat_id, prepared.room_projection.as_ref()).unwrap();
    let rpc = core.rpc_service();
    rpc.install_scaffold_control_grant(&prepared).unwrap();
    let command = SessionCommandPayload::Control {
        session_id: chat_id.clone(),
        owner_device_id: prepared.attached_device_id.clone().unwrap(),
        actor_device_id: core.device_id.clone(),
        actor_subject: "owner@example.com".into(),
        grant_id: prepared.control_grant.as_ref().unwrap().id.clone(),
        source: comet_proto::AgentSessionSource::Scaffold,
        action: Box::new(comet_doc::SessionControlAction::Queue {
            prompt: "queued instruction".into(), message_id: None,
        }),
    };
    let request = serde_json::json!({
        "chatId": chat_id, "commandId": "exact-deployment", "command": command,
        "deploymentId": "deployment-a", "controlDeploymentId": "deployment-a",
        "roomProjection": prepared.room_projection,
        "targetDeviceId": prepared.attached_device_id,
    });
    let mut foreign = request.clone();
    foreign["commandId"] = serde_json::json!("foreign-deployment");
    foreign["deploymentId"] = serde_json::json!("deployment-b");
    foreign["controlDeploymentId"] = serde_json::json!("deployment-b");
    foreign["roomProjection"]["deploymentId"] = serde_json::json!("deployment-b");
    assert!(rpc.handle(methods::QUEUE_COMMAND, foreign).await.is_err());
    assert!(core.doc_host.command_entry(&chat_id, "foreign-deployment").unwrap().is_none());
    let mut wrong_owner = request.clone();
    wrong_owner["commandId"] = serde_json::json!("wrong-owner");
    wrong_owner["targetDeviceId"] = serde_json::json!("comet-scaffold-other-sandbox-e2");
    assert!(rpc.handle(methods::QUEUE_COMMAND, wrong_owner).await.is_err());
    assert!(core.doc_host.command_entry(&chat_id, "wrong-owner").unwrap().is_none());
    rpc.handle(methods::QUEUE_COMMAND, request).await.unwrap();
    assert_eq!(core.doc_host.command_entry(&chat_id, "exact-deployment").unwrap().unwrap().payload, command);
}


fn recovery_core(path: &std::path::Path, origin: &str) -> crate::EngineCore {
    let core = crate::EngineCore::assemble_with_identity(
        path,
        std::sync::Arc::new(crate::HarnessRegistry::new()),
        HarnessId::Omp,
        None,
        "project-a",
        "owner@example.com",
        RuntimeProfile::LocalController,
    )
    .unwrap();
    let mut auth = crate::AuthConfig::new(origin, path);
    auth.project_scope = "project-a".into();
    auth.dev_user_id = "owner@example.com".into();
    core.set_auth(crate::Auth::new(auth));
    core.set_scaffold_runtime(
        crate::ScaffoldRuntime::new(
            crate::ScaffoldClient::new(
                origin,
                "project-a",
                std::sync::Arc::new(comet_rpc::StaticToken("test".into())),
            )
            .unwrap(),
            origin,
            std::sync::Arc::new(crate::UnavailableDeviceJoinGrantProvider),
        )
        .with_deployment_id("deployment-a".into()),
    );
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
    let mut source = core.workspace.doc().chat(source_id).unwrap().unwrap();
    source.harness_session_id = Some("native-source".into());
    source.harness_session_cwd = Some("/source".into());
    core.workspace.doc().upsert_chat(&source).unwrap();
    core
}

#[tokio::test]
async fn scaffold_attach_verifies_authority_before_opening_a_legacy_cache() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let scope = params().scope;
    let chat_id = scope.session_id.as_deref().unwrap();
    let store = comet_sync::DocsStore::open(dir.path().join("projects")
        .join(crate::sanitize_path_id("project-a"))
        .join(crate::sanitize_path_id("owner@example.com"))).unwrap();
    let legacy = comet_doc::SessionDoc::init(chat_id).unwrap();
    legacy.push_message(&comet_doc::SessionMessageEntry {
        id: "retained-output".into(), role: comet_doc::MessageRole::Assistant,
        parts: vec![comet_doc::MessagePart::Text { id: "text".into(), text: "accepted output".into() }],
        created_at: 1, device_id: "comet-scaffold-sandbox-a-e1".into(),
        status: Some(comet_doc::MessageStatus::Complete), continuation_of: None, peer_message: None,
    }).unwrap();
    store.save_snapshot(chat_id, &legacy.export_snapshot().unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let core = recovery_core(dir.path(), &format!("http://{}", listener.local_addr().unwrap()));
    let environment = serde_json::json!({"sandbox": {
        "id": "sandbox-a", "lifecycleEpoch": 1, "status": "ready", "kind": "remote_code",
        "runtimeProfile": "remote_code", "ownerEmail": "owner@example.com",
        "createdAt": "2026-08-04T00:00:00Z", "updatedAt": "2026-08-04T00:00:00Z"
    }}).to_string();
    let authority = serde_json::json!({"ok": true, "exitCode": 0, "stdout": serde_json::json!({
        "grantId": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "expiresAt": crate::now_ms() + 60_000,
        "principalSubject": "owner@example.com", "scope": scope,
        "sandboxId": "sandbox-a", "deviceId": "comet-scaffold-sandbox-a-e1", "lifecycleEpoch": 1,
        "capabilities": ["session.read", "session.chat", "session.control", "session.annotate", "session.files", "session.environment"]
    }).to_string()}).to_string();
    let provider = tokio::spawn(async move {
        for (method, body) in [("GET", environment), ("POST", authority)] {
            let (connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await.unwrap().unwrap();
            let mut reader = BufReader::new(connection);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert!(line.starts_with(&format!("{method} /api/code-sandboxes/sandbox-a")));
            let mut length = 0;
            loop {
                line.clear(); reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" { break; }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut request = vec![0; length];
            reader.read_exact(&mut request).await.unwrap();
            if method == "POST" {
                let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
                assert_eq!(request["argv"], serde_json::json!(["comet", "scaffold-authority"]));
            }
            reader.get_mut().write_all(format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()
            ).as_bytes()).await.unwrap();
        }
    });
    let cancellation = CancellationToken::new();
    let operation_cancel = cancellation.clone();
    let rpc = core.rpc_service();
    let control = ScaffoldEnvironmentControl::Attach { sandbox_id: "sandbox-a".into(), scope: scope.clone() };
    let operation = tokio::spawn(async move { rpc.control_scaffold_environment(control, &operation_cancel).await });
    provider.await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while store.document_scope(chat_id).unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.unwrap();
    cancellation.cancel();
    assert!(operation.await.unwrap().unwrap_err().to_string().contains("scaffold_request_cancelled"));
    let handle = core.doc_host.open_projection(chat_id, Some(&SessionRoomProjection {
        project_id: scope.project_id, deployment_id: scope.deployment_id.unwrap(), session_id: chat_id.into(),
    })).unwrap();
    assert_eq!(handle.doc().read_entries().unwrap(), legacy.read_entries().unwrap());
    core.shutdown().await;
}

fn recovery_request() -> comet_rpc::RecoverSessionHandoffToScaffoldParams {
    comet_rpc::RecoverSessionHandoffToScaffoldParams {
        handoff: HandoffSessionToScaffoldParams {
            source_chat_id: "00000000-0000-4000-8000-000000000002".into(),
            prompt: "Continue remotely".into(),
            database_environment: ScaffoldDatabaseEnvironment::Local,
        },
        recover_chat_id: params().scope.session_id.unwrap(),
        recover_sandbox_id: "sandbox-a".into(),
    }
}

async fn respond_to_route_receipt(connection: tokio::net::TcpStream, model: &str) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut reader = BufReader::new(connection);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let supported = line.trim() == "GET /api/code-sandboxes/sandbox-a HTTP/1.1";
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
    }
    let body = serde_json::json!({ "ok": true, "sandbox": {
        "id": "sandbox-a", "kind": "remote_code", "runtimeProfile": "comet_remote",
        "status": "ready", "lifecycleEpoch": 1, "ownerEmail": "owner@example.com",
        "databaseEnvironment": "local", "createdAt": "2026-08-10T00:00:00Z", "updatedAt": "2026-08-10T00:00:00Z",
        "cometRuntimeProfile": { "version": "scaffold.comet-runtime.v1", "projectId": "project-a",
            "deploymentId": "deployment-a", "sessionId": recovery_request().recover_chat_id, "sandboxId": "sandbox-a" },
        "agentRoute": { "provider": "openai", "model": model, "fallback": "disabled", "routingMode": "automatic" }
    }}).to_string();
    let status = if supported { "200 OK" } else { "404 Not Found" };
    reader.get_mut().write_all(format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
}

#[tokio::test]
async fn recovery_route_mismatch_never_attaches_imports_creates_or_commands() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let core = recovery_core(
        dir.path(),
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    core.workspace
        .upsert_session_ref(
            &recovery_request().recover_chat_id,
            Some(response(&params().scope, ScaffoldLifecycle::Ready).environment),
        )
        .unwrap();
    let rpc = core.rpc_service();
    let (result, ()) = tokio::join!(
        rpc.recover_session_handoff_to_scaffold(recovery_request()),
        async {
            let (connection, _) = listener.accept().await.unwrap();
            respond_to_route_receipt(connection, "different-model").await;
        }
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("agent route differs")
    );
    assert!(
        !core
            .doc_host
            .chat_has_commands(&recovery_request().recover_chat_id)
            .unwrap()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), listener.accept())
            .await
            .is_err()
    );
    core.shutdown().await;
}

#[tokio::test]
async fn recovery_reference_disappearing_or_changing_during_validation_never_creates() {
    for remove in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let core = recovery_core(
            dir.path(),
            &format!("http://{}", listener.local_addr().unwrap()),
        );
        let mut accepted = response(&params().scope, ScaffoldLifecycle::Ready).environment;
        let id = recovery_request().recover_chat_id;
        core.workspace
            .upsert_session_ref(&id, Some(accepted.clone()))
            .unwrap();
        let rpc = core.rpc_service();
        let first = tokio::spawn(async move {
            rpc.recover_session_handoff_to_scaffold(recovery_request())
                .await
        });
        let (connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        if remove {
            core.workspace
                .doc()
                .remove_session_ref("owner@example.com", &id)
                .unwrap();
        } else {
            accepted.database_environment = Some(ScaffoldDatabaseEnvironment::ProductionSnapshot);
            core.workspace
                .upsert_session_ref(&id, Some(accepted))
                .unwrap();
        }
        respond_to_route_receipt(connection, "gpt-6-astra").await;
        let error = tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("recovery"));
        assert!(!core.doc_host.chat_has_commands(&id).unwrap());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), listener.accept())
                .await
                .is_err()
        );
        core.shutdown().await;
    }
}

#[tokio::test]
async fn imported_context_without_command_admission_is_persisted_and_not_reimported() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let core = recovery_core(
        dir.path(),
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    let mut attached = response(&params().scope, ScaffoldLifecycle::Ready);
    attached.handoff_native_session_id = Some("native-source".into());
    attached.handoff_cwd = Some("/workspace/ashler-platform".into());
    core.workspace
        .upsert_session_ref(
            &recovery_request().recover_chat_id,
            Some(attached.environment.clone()),
        )
        .unwrap();
    let source = core
        .workspace
        .doc()
        .chat(&recovery_request().handoff.source_chat_id)
        .unwrap()
        .unwrap();
    core.rpc_service()
        .persist_handoff_context(
            &source,
            params().agent_route.omp_model(),
            attached.attached_device_id.as_deref().unwrap(),
            &attached,
        )
        .unwrap();
    let error = core
        .rpc_service()
        .recover_session_handoff_to_scaffold(recovery_request())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("already imported native context")
    );
    assert!(
        !core
            .doc_host
            .chat_has_commands(&recovery_request().recover_chat_id)
            .unwrap()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), listener.accept())
            .await
            .is_err()
    );
    core.shutdown().await;
}

#[tokio::test]
async fn explicit_recovery_missing_mismatched_imported_or_admitted_target_never_creates_or_commands()
 {
    for case in [
        "missing", "sandbox", "scope", "owner", "database", "active", "imported", "admitted",
        "ledger",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let core = recovery_core(
            dir.path(),
            &format!("http://{}", listener.local_addr().unwrap()),
        );
        let request = recovery_request();
        let id = request.recover_chat_id.clone();
        if case != "missing" {
            let mut environment = response(&params().scope, ScaffoldLifecycle::Ready).environment;
            match case {
                "sandbox" => {
                    if let SessionEnvironmentSource::Scaffold { sandbox_id, .. } =
                        &mut environment.source
                    {
                        *sandbox_id = "other".into();
                    }
                }
                "scope" => environment.scope.deployment_id = Some("other".into()),
                "owner" => environment.owner_principal = "other@example.com".into(),
                "database" => {
                    environment.database_environment =
                        Some(ScaffoldDatabaseEnvironment::StagingSnapshot)
                }
                "active" => {
                    if let SessionEnvironmentSource::Scaffold { lifecycle, .. } =
                        &mut environment.source
                    {
                        *lifecycle = ScaffoldLifecycle::AgentRunning;
                    }
                }
                _ => {}
            }
            core.workspace
                .upsert_session_ref(&id, Some(environment))
                .unwrap();
            if case == "imported" {
                let mut chat = core
                    .workspace
                    .doc()
                    .chat(&request.handoff.source_chat_id)
                    .unwrap()
                    .unwrap();
                chat.id = id.clone();
                core.workspace.doc().upsert_chat(&chat).unwrap();
            }
            if case == "admitted" {
                core.workspace
                    .update_session_startup(
                        &id,
                        None,
                        comet_proto::SessionStartup {
                            generation: "accepted".into(),
                            status: comet_proto::SessionStartupStatus::Admitted,
                            updated_at: chrono::Utc::now(),
                            command_id: Some("command-a".into()),
                        },
                    )
                    .unwrap();
            }
            if case == "ledger" {
                core.doc_host
                    .queue_command(
                        &id,
                        SessionCommandPayload::Run {
                            request: RunRequest {
                                prompt: "existing".into(),
                                model: None,
                                agent_account_id: None,
                                reasoning: None,
                                model_options: Default::default(),
                                cwd: "/source".into(),
                                sandbox: SandboxLevel::WorkspaceWrite,
                                auto_approve: false,
                                resume: None,
                                attachments: Vec::new(),
                            },
                            message_id: crate::new_id(),
                        },
                    )
                    .unwrap();
            }
        }
        let before = core.doc_host.chat_has_commands(&id).unwrap();
        let client = comet_rpc::memory_client(core.rpc_service());
        let result = client
            .call(
                comet_rpc::methods::RECOVER_SESSION_HANDOFF_TO_SCAFFOLD,
                serde_json::to_value(request).unwrap(),
            )
            .await;
        assert!(
            result.unwrap_err().to_string().contains("recovery"),
            "{case}"
        );
        assert_eq!(
            core.doc_host.chat_has_commands(&id).unwrap(),
            before,
            "{case}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), listener.accept())
                .await
                .is_err(),
            "{case}"
        );
        core.shutdown().await;
    }
}

#[tokio::test]
async fn concurrent_recovery_rejects_same_target_before_transfer_and_admission() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let core = recovery_core(
        dir.path(),
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    let environment = response(&params().scope, ScaffoldLifecycle::Ready).environment;
    core.workspace
        .upsert_session_ref(&recovery_request().recover_chat_id, Some(environment))
        .unwrap();
    let rpc = core.rpc_service();
    let first = tokio::spawn(async move {
        rpc.recover_session_handoff_to_scaffold(recovery_request())
            .await
    });
    let (connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let client = comet_rpc::memory_client(core.rpc_service());
    let admission = |generation: Option<String>| {
        client.call(comet_rpc::methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": recovery_request().recover_chat_id,
                "preparationGeneration": generation,
                "command": SessionCommandPayload::Run {
                    request: RunRequest { prompt: "Competing start".into(), model: None, agent_account_id: None,
                        reasoning: None, model_options: Default::default(), cwd: "/source".into(),
                        sandbox: SandboxLevel::WorkspaceWrite, auto_approve: false, resume: None, attachments: Vec::new() },
                    message_id: crate::new_id(),
                },
            }))
    };
    assert!(
        admission(None)
            .await
            .unwrap_err()
            .to_string()
            .contains("already in progress")
    );
    respond_to_route_receipt(connection, "gpt-6-astra").await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if core
                .workspace
                .session_startup(&recovery_request().recover_chat_id)
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // The first operation is allowed to inspect its preserved sandbox while
    // competitors remain fenced; no owner-room connection is a prerequisite.
    let (connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await.unwrap().unwrap();
    let mut attachment = BufReader::new(connection);
    let mut request = String::new();
    attachment.read_line(&mut request).await.unwrap();
    assert!(request.starts_with("GET /api/code-sandboxes/sandbox-a"), "{request:?}");
    assert!(
        !first.is_finished(),
        "first recovery must retain its target gate while waiting for attachment"
    );
    assert!(
        core.rpc_service()
            .recover_session_handoff_to_scaffold(recovery_request())
            .await
            .unwrap_err()
            .to_string()
            .contains("already in progress")
    );
    let generation = core
        .workspace
        .session_startup(&recovery_request().recover_chat_id)
        .unwrap()
        .unwrap()
        .generation;
    assert!(
        admission(Some(generation))
            .await
            .unwrap_err()
            .to_string()
            .contains("already in progress")
    );
    assert!(
        !core
            .doc_host
            .chat_has_commands(&recovery_request().recover_chat_id)
            .unwrap()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), listener.accept())
            .await
            .is_err()
    );
    first.abort();
    let _ = first.await;
    core.shutdown().await;
}

#[tokio::test]
async fn recovery_peer_diagnostic_does_not_duplicate_admission() {
    let dir = tempfile::tempdir().unwrap();
    let core = recovery_core(dir.path(), "http://127.0.0.1:1");
    let mut parameters = params();
    parameters.database_environment = ScaffoldDatabaseEnvironment::StagingSnapshot;
    let scope = parameters.scope.clone();
    let mut accepted = response(&scope, ScaffoldLifecycle::Ready).environment;
    accepted.database_environment = Some(ScaffoldDatabaseEnvironment::StagingSnapshot);
    core.workspace
        .upsert_session_ref(scope.session_id.as_deref().unwrap(), Some(accepted.clone()))
        .unwrap();
    // This is an already accepted diagnostic in the scoped replica, not a new
    // bare control admission inheriting deployment authority from its cache.
    core.doc_host
        .open_projection(scope.session_id.as_deref().unwrap(), response(&scope, ScaffoldLifecycle::Ready).room_projection.as_ref())
        .unwrap().doc().queue_command(&comet_doc::SessionCommandEntry {
            id:"diagnostic".into(), issued_by:core.device_id.clone(), issued_at:1,
            based_on:None, expires_at:None, status:comet_doc::SessionCommandStatus::Applied, resolution:None,
            payload:SessionCommandPayload::PeerMessage {
                text:"Read-only diagnostic".into(), source_chat_id:recovery_request().handoff.source_chat_id,
                source_deployment_id:None, source_device_id:None, thread_id:"diagnostic".into(),
                reply_to:None, hop_count:0,
            },
        }).unwrap();
    core.rpc_service()
        .require_unadmitted_handoff(&recovery_request().recover_chat_id)
        .unwrap();
    let mut startup =
        PreparationOutcome::begin(core.workspace.clone(), scope.session_id.as_deref().unwrap())
            .unwrap();
    let mut attached = prepare_scaffold_session_with(parameters, Some(accepted), |operation| {
        let mut result = response(&scope, ScaffoldLifecycle::Ready);
        result.environment.database_environment =
            Some(ScaffoldDatabaseEnvironment::StagingSnapshot);
        match operation {
            ScaffoldEnvironmentControl::Attach {
                sandbox_id,
                scope: actual,
            }
            | ScaffoldEnvironmentControl::Inspect {
                sandbox_id,
                scope: actual,
            } => {
                assert_eq!(sandbox_id, "sandbox-a");
                assert_eq!(actual, scope);
            }
            ScaffoldEnvironmentControl::HandoffOmpSession { sandbox_id, .. } => {
                assert_eq!(sandbox_id, "sandbox-a");
                result.handoff_native_session_id = Some("native-source".into());
                result.handoff_cwd = Some("/workspace/ashler-platform".into());
            }
            _ => panic!("recovery must never create or update the database/route"),
        }
        std::future::ready(Ok(result))
    })
    .await
    .unwrap();
    startup.armed = false;
    attached.preparation_generation = Some(startup.startup.generation.clone());
    let source = core
        .workspace
        .doc()
        .chat(&recovery_request().handoff.source_chat_id)
        .unwrap()
        .unwrap();
    let receipt = core
        .rpc_service()
        .admit_scaffold_handoff(
            source,
            "Continue".into(),
            params().agent_route.omp_model(),
            "owner@example.com".into(),
            attached,
        )
        .unwrap();
    let mut replay = recovery_request();
    replay.handoff.database_environment = ScaffoldDatabaseEnvironment::StagingSnapshot;
    assert!(
        core.rpc_service()
            .recover_session_handoff_to_scaffold(replay)
            .await
            .unwrap_err()
            .to_string()
            .contains("recovery")
    );
    assert_eq!(
        core.doc_host
            .open(&receipt.chat_id)
            .unwrap()
            .doc()
            .read_commands()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        core.workspace
            .session_startup(&receipt.chat_id)
            .unwrap()
            .unwrap()
            .command_id
            .as_deref(),
        Some(receipt.command_id.as_str())
    );
    core.shutdown().await;
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
    assert!(matches!(
        result.environment.source,
        SessionEnvironmentSource::Scaffold {
            lifecycle: ScaffoldLifecycle::Ready,
            ..
        }
    ));
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
        "scaffold_request_failed: connection_reset_or_closed",
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
    let (connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await.unwrap().unwrap();
    let mut initial_attach = BufReader::new(connection);
    line.clear();
    initial_attach.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("GET /api/code-sandboxes/sandbox-a"));
    first.abort();
    let _ = first.await;
    let retry_rpc = core.rpc_service();
    tokio::select! {
        result = retry_rpc.prepare_scaffold_session(params()) => {
            panic!("recovery should inspect the same target before attachment: {result:?}");
        }
        accepted = async {
            loop {
                let mut retry_attach = BufReader::new(listener.accept().await.unwrap().0);
                line.clear();
                // Cancellation can leave an accepted TCP connection with no request.
                if retry_attach.read_line(&mut line).await.unwrap() != 0 { break line.clone(); }
            }
        } => {
            assert!(accepted.starts_with("GET /api/code-sandboxes/sandbox-a"), "{accepted:?}");
        }
        _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("retry did not inspect the preserved sandbox"),
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

struct HandoffHarness {
    requests: tokio::sync::mpsc::UnboundedSender<(RunRequest, String)>,
    steers: tokio::sync::mpsc::UnboundedSender<comet_harness::SteerMessage>,
    interrupts: tokio::sync::mpsc::UnboundedSender<String>,
    answers: tokio::sync::mpsc::UnboundedSender<(String, Vec<comet_proto::UserInputAnswer>)>,
}

#[async_trait::async_trait]
impl comet_harness::Harness for HandoffHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Omp
    }
    fn display_name(&self) -> &str {
        "Handoff recording harness"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> comet_proto::SteeringMode {
        comet_proto::SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[comet_proto::ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<comet_proto::Model>, comet_harness::HarnessError> {
        Ok(Vec::new())
    }
    async fn run(
        &self,
        request: RunRequest,
        mut controls: comet_harness::RunControls,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<comet_proto::AgentEvent, comet_harness::HarnessError>,
        >,
        comet_harness::HarnessError,
    > {
        let session_id = controls.context.as_ref().unwrap().session_id.clone();
        self.requests.send((request.clone(), session_id)).unwrap();
        let steers = self.steers.clone();
        let interrupts = self.interrupts.clone();
        let answers = self.answers.clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let native_id = request
                .resume
                .expect("handoff must preserve native identity");
            let _ = tx.send(Ok(comet_proto::AgentEvent::SessionStarted {
                harness: HarnessId::Omp,
                model: request.model.unwrap(),
                tools: Vec::new(),
                cwd: request.cwd,
                session_id: native_id.clone(),
                assistant_message_id: "handoff-assistant".into(),
            }));
            loop {
                tokio::select! {
                    biased;
                    _ = controls.interrupt.cancelled() => {
                        let _ = interrupts.send(native_id.clone());
                        break;
                    },
                    message = controls.steering.recv() => {
                        let Some(message) = message else { break; };
                        if steers.send(message).is_err() { break; }
                        let received = (controls.request_input)(vec![comet_proto::UserInputQuestion {
                            id: "next-step".into(),
                            header: "Continue".into(),
                            question: "Which step should run next?".into(),
                            options: vec!["Verify".into(), "Finish".into()],
                            multi_select: false,
                        }]).await.expect("live input answer");
                        answers.send((native_id.clone(), received)).unwrap();
                    }
                }
            }
            let _ = tx.send(Ok(comet_proto::AgentEvent::Done {
                status: comet_proto::DoneStatus::Interrupted,
                result: None,
                error: None,
                session_id: Some(native_id),
            }));
        });
        Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })))
    }
}

#[tokio::test]
async fn prepared_handoff_routes_peer_messages_and_interrupts_to_the_remote_native_writer() {
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
    assert!(
        core.rpc_service()
            .admit_scaffold_handoff(
                source.clone(),
                "Stale task".into(),
                "openai-codex/gpt-6-astra".into(),
                "owner@example.com".into(),
                stale,
            )
            .is_err()
    );
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
    let admitted = core
        .workspace
        .session_startup(&receipt.chat_id)
        .unwrap()
        .unwrap();
    assert_eq!(admitted.generation, startup.startup.generation);
    assert_eq!(admitted.status, comet_proto::SessionStartupStatus::Admitted);
    assert_eq!(
        admitted.command_id.as_deref(),
        Some(receipt.command_id.as_str())
    );
    let local_handle = core.doc_host.open(&receipt.chat_id).unwrap();
    let start_snapshot = local_handle.doc().export_snapshot().unwrap();

    // A peer send must remain pending on the controller even though the source
    // chat and transferred native identity both originated on this device.
    let client = comet_rpc::memory_client(core.rpc_service());
    let peer_params = serde_json::json!({
        "sourceChatId": source_id,
        "targetChatId": receipt.chat_id,
        "commandId": "handoff-peer",
        "text": "Continue remotely, without starting another writer",
    });
    client
        .call(methods::SEND_PEER_MESSAGE, peer_params.clone())
        .await
        .unwrap();
    core.doc_host.drain_commands(&local_handle).await;
    assert_eq!(
        core.doc_host
            .command_entry(&receipt.chat_id, "handoff-peer")
            .unwrap()
            .unwrap()
            .status,
        comet_doc::SessionCommandStatus::Pending,
    );

    // The sandbox joins the same project workspace and receives the session
    // document separately with its verified writer authority.
    let remote_dir = tempfile::tempdir().unwrap();
    std::fs::write(remote_dir.path().join("device-id"), &owner_device_id).unwrap();
    let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
    let (steers_tx, mut steers_rx) = tokio::sync::mpsc::unbounded_channel();
    let (interrupts_tx, mut interrupts_rx) = tokio::sync::mpsc::unbounded_channel();
    let (answers_tx, mut answers_rx) = tokio::sync::mpsc::unbounded_channel();
    let registry = crate::HarnessRegistry::for_profile(RuntimeProfile::ScaffoldHost);
    registry.register(std::sync::Arc::new(HandoffHarness {
        requests: requests_tx,
        steers: steers_tx,
        interrupts: interrupts_tx,
        answers: answers_tx,
    }));
    let remote_core = crate::EngineCore::assemble_with_identity(
        remote_dir.path(),
        std::sync::Arc::new(registry),
        HarnessId::Omp,
        None,
        "project-a",
        "owner@example.com",
        RuntimeProfile::ScaffoldHost,
    )
    .unwrap();
    remote_core
        .workspace
        .doc()
        .doc()
        .import(&core.workspace.doc().export_snapshot().unwrap())
        .unwrap();
    let grant = core
        .doc_host
        .collaboration_grants("owner@example.com", &[receipt.chat_id.clone()])
        .pop()
        .expect("validated controller attachment grant");
    let envelope = comet_proto::VerifiedCapabilityGrantEnvelope {
        grant,
        room_id: format!("s4/project-a/deployment-a/{}", receipt.chat_id),
        target_device_id: owner_device_id,
        target_session_id: receipt.chat_id.clone(),
        unknown: Default::default(),
    };
    remote_core
        .doc_host
        .ingest_verified_grant(&receipt.chat_id, &serde_json::to_vec(&envelope).unwrap())
        .unwrap();
    let remote_handle = remote_core.doc_host.open(&receipt.chat_id).unwrap();
    remote_handle.doc().doc().import(&start_snapshot).unwrap();
    let (started, child_session_id) =
        tokio::time::timeout(Duration::from_secs(5), requests_rx.recv())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(started.resume.as_deref(), Some("native-source"));
    // Sidebar state travels through the workspace, without a transcript watch.
    async fn sync_sidebar_workspace(
        controller: &crate::EngineCore,
        remote: &crate::EngineCore,
        chat_id: &str,
        status: SessionStatus,
    ) -> (Chat, comet_proto::Session) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let chat = remote.workspace.doc().chat(chat_id).unwrap().unwrap();
                let session = remote
                    .workspace
                    .doc()
                    .read_sessions()
                    .unwrap()
                    .into_iter()
                    .find(|session| session.chat_id == chat_id);
                if let Some(session) = session
                    && session.status == status
                    && chat.last_message_at.is_some_and(|at| {
                        !matches!(status, SessionStatus::Idle | SessionStatus::Errored)
                            || at.timestamp_millis() >= session.updated_at.timestamp_millis()
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        controller
            .workspace
            .doc()
            .doc()
            .import(&remote.workspace.doc().export_snapshot().unwrap())
            .unwrap();
        let chat = controller.workspace.doc().chat(chat_id).unwrap().unwrap();
        let session = controller
            .workspace
            .doc()
            .read_sessions()
            .unwrap()
            .into_iter()
            .find(|session| session.chat_id == chat_id)
            .unwrap();
        assert_eq!(session.status, status);
        (chat, session)
    }
    let (working_chat, working) = sync_sidebar_workspace(
        &core,
        &remote_core,
        &receipt.chat_id,
        SessionStatus::Working,
    )
    .await;
    assert_eq!(
        comet_proto::view::display_status(&working_chat, Some(&working), chrono::Utc::now()),
        comet_proto::ChatIndicator::Working
    );
    assert_eq!(started.cwd, "/workspace/ashler-platform");
    remote_handle
        .doc()
        .doc()
        .import(&local_handle.doc().export_snapshot().unwrap())
        .unwrap();
    let steered = tokio::time::timeout(Duration::from_secs(5), steers_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(steered.message_id.as_deref(), Some("handoff-peer"));
    assert!(
        steered
            .prompt
            .contains("Continue remotely, without starting another writer")
    );
    assert!(
        requests_rx.try_recv().is_err(),
        "peer delivery must not launch a second native writer"
    );
    let remote_chat = remote_core
        .workspace
        .doc()
        .chat(&receipt.chat_id)
        .unwrap()
        .unwrap();
    assert_eq!(remote_chat.device_id, remote_core.device_id);
    assert!(remote_core.workspace.is_host(&receipt.chat_id));
    assert_eq!(
        remote_core
            .doc_host
            .command_entry(&receipt.chat_id, "handoff-peer")
            .unwrap()
            .unwrap()
            .status,
        comet_doc::SessionCommandStatus::Applied,
    );

    // Replaying the controller request and its CRDT history cannot re-deliver it.
    client
        .call(methods::SEND_PEER_MESSAGE, peer_params)
        .await
        .unwrap();
    remote_handle
        .doc()
        .doc()
        .import(&local_handle.doc().export_snapshot().unwrap())
        .unwrap();
    remote_core.doc_host.drain_commands(&remote_handle).await;
    assert!(requests_rx.try_recv().is_err());
    assert!(steers_rx.try_recv().is_err());

    // The id exported to the native child must remain a usable Crew session
    // address, not the engine's private per-agent execution key.
    let remote_client = comet_rpc::memory_client(remote_core.rpc_service());
    let reply = remote_client
        .call(
            methods::REPLY_PEER_MESSAGE,
            serde_json::json!({
                "sessionId": child_session_id,
                "commandId": "handoff-peer",
                "text": "Reply from the resumed native session",
            }),
        )
        .await
        .unwrap();
    assert_eq!(reply["threadId"], "handoff-peer");
    let reply_command = remote_core
        .doc_host
        .command_entry(source_id, "reply:handoff-peer")
        .unwrap()
        .unwrap();
    assert!(
        matches!(reply_command.payload, SessionCommandPayload::PeerMessage {
        source_chat_id, reply_to: Some(reply_to), ..
    } if source_chat_id == receipt.chat_id && reply_to == "handoff-peer")
    );

    // Answer the question raised by the peer-steered native writer through
    // the host-local legacy RPC, using its public canonical chat id.
    let mut input_updates = remote_handle.watch_messages();
    let input_request_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let request_id = remote_handle
                .doc()
                .read_entries()
                .unwrap()
                .iter()
                .find_map(|entry| {
                    entry.parts.iter().find_map(|part| match part {
                        comet_doc::MessagePart::Input {
                            request_id,
                            resolved: false,
                            ..
                        } => Some(request_id.clone()),
                        _ => None,
                    })
                });
            if let Some(request_id) = request_id {
                break request_id;
            }
            input_updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let (waiting_chat, waiting) = sync_sidebar_workspace(
        &core,
        &remote_core,
        &receipt.chat_id,
        SessionStatus::AwaitingInput,
    )
    .await;
    assert_eq!(
        comet_proto::view::display_status(&waiting_chat, Some(&waiting), chrono::Utc::now()),
        comet_proto::ChatIndicator::AwaitingInput
    );
    core.workspace
        .mark_chat_seen(&receipt.chat_id, chrono::Utc::now())
        .unwrap();
    remote_client
        .call(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": child_session_id,
                "commandId": "handoff-answer",
                "command": {
                    "kind": "respondInput",
                    "requestId": input_request_id,
                    "answers": [{ "questionId": "next-step", "labels": ["Verify"] }],
                },
            }),
        )
        .await
        .unwrap();
    let (answer_native_id, received_answers) =
        tokio::time::timeout(Duration::from_secs(5), answers_rx.recv())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(answer_native_id, "native-source");
    assert_eq!(
        received_answers,
        vec![comet_proto::UserInputAnswer {
            question_id: "next-step".into(),
            labels: vec!["Verify".into()],
        }]
    );
    assert!(
        requests_rx.try_recv().is_err(),
        "answer must not launch another writer"
    );

    // The host-local legacy RPC is authorized, but must interrupt the assigned
    // native writer rather than silently succeeding against an empty bare key.
    remote_client
        .call(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": child_session_id,
                "commandId": "handoff-interrupt",
                "command": { "kind": "interrupt" },
            }),
        )
        .await
        .unwrap();
    remote_core.doc_host.drain_commands(&remote_handle).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), interrupts_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        "native-source",
    );
    let (completed_chat, completed) =
        sync_sidebar_workspace(&core, &remote_core, &receipt.chat_id, SessionStatus::Idle).await;
    assert_eq!(
        comet_proto::view::display_status(&completed_chat, Some(&completed), chrono::Utc::now()),
        comet_proto::ChatIndicator::Completed
    );
    core.workspace
        .mark_chat_seen(&receipt.chat_id, chrono::Utc::now())
        .unwrap();
    let seen_chat = core
        .workspace
        .doc()
        .chat(&receipt.chat_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        comet_proto::view::display_status(&seen_chat, Some(&completed), chrono::Utc::now()),
        comet_proto::ChatIndicator::Idle
    );
    assert_eq!(
        comet_proto::view::effective_indicator(
            Some(&working),
            working.updated_at
                + chrono::Duration::milliseconds(comet_proto::view::SESSION_STALE_MS + 1)
        ),
        comet_proto::view::Indicator::Unreachable
    );
    // The persisted workspace alone retains completion and seen state; no
    // session document or scoped follower is needed after restart.
    let restored_dir = tempfile::tempdir().unwrap();
    let restored_store =
        std::sync::Arc::new(comet_sync::DocsStore::open(restored_dir.path()).unwrap());
    restored_store
        .save_snapshot(
            crate::workspace_host::WORKSPACE_DOC_ID,
            &core.workspace.doc().export_snapshot().unwrap(),
        )
        .unwrap();
    let restored_workspace = crate::WorkspaceHost::open(
        restored_store,
        crate::WorkspaceHostConfig {
            device_id: core.device_id.clone(),
            device_name: "restored".into(),
            platform: "test".into(),
            project_scope: "project-a".into(),
            user_id: "owner@example.com".into(),
            edge: None,
        },
    )
    .unwrap();
    let restored_session = restored_workspace
        .doc()
        .read_sessions()
        .unwrap()
        .into_iter()
        .find(|session| session.chat_id == receipt.chat_id)
        .unwrap();
    assert_eq!(restored_session.status, SessionStatus::Idle);
    let restored_chat = restored_workspace
        .doc()
        .chat(&receipt.chat_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        restored_chat.last_message_at,
        completed_chat.last_message_at
    );
    assert_eq!(
        comet_proto::view::display_status(
            &restored_chat,
            Some(&restored_session),
            chrono::Utc::now()
        ),
        comet_proto::ChatIndicator::Idle
    );

    // Chats without an assigned agent session still use the bare chat writer.
    let bare_chat_id = "00000000-0000-4000-8000-000000000003";
    let mut bare_request = started;
    bare_request.resume = Some("native-bare".into());
    remote_core
        .doc_host
        .queue_command(
            bare_chat_id,
            SessionCommandPayload::Run {
                request: bare_request,
                message_id: "bare-run".into(),
            },
        )
        .unwrap();
    let (_, bare_child_session_id) =
        tokio::time::timeout(Duration::from_secs(5), requests_rx.recv())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(bare_child_session_id, bare_chat_id);
    remote_client
        .call(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": bare_chat_id,
                "command": { "kind": "interrupt" },
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), interrupts_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        "native-bare",
    );
    remote_core.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn initial_queue_requires_exact_preparation_but_admitted_followups_do_not() {
    use comet_proto::{SessionStartup, SessionStartupStatus};
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
    let chat_id = params().scope.session_id.unwrap();
    core.workspace
        .create_space("space", &core.device_id, "/workspace", None, false)
        .unwrap();
    core.workspace
        .create_chat(&chat_id, "space", None, None)
        .unwrap();
    let preparing = SessionStartup {
        generation: "new-attempt".into(),
        status: SessionStartupStatus::Preparing,
        updated_at: chrono::Utc::now(),
        command_id: None,
    };
    core.workspace
        .update_session_startup(&chat_id, None, preparing.clone())
        .unwrap();
    let request = RunRequest {
        prompt: "Start once".into(),
        model: None,
        agent_account_id: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/workspace".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: None,
        attachments: Vec::new(),
    };
    let run = SessionCommandPayload::Run {
        request: request.clone(),
        message_id: "message".into(),
    };
    let control = SessionCommandPayload::Control {
        session_id: chat_id.clone(),
        owner_device_id: "remote-owner".into(),
        actor_device_id: core.device_id.clone(),
        actor_subject: "owner@example.com".into(),
        grant_id: "grant".into(),
        source: AgentSessionSource::Scaffold,
        action: Box::new(comet_doc::SessionControlAction::Start {
            request,
            message_id: "remote-message".into(),
        }),
    };
    let rpc = core.rpc_service();
    for command in [&run, &control] {
        for generation in [None, Some("old-attempt")] {
            assert!(
                rpc.handle(
                    methods::QUEUE_COMMAND,
                    serde_json::json!({
                        "chatId": chat_id, "commandId": "rejected", "command": command,
                        "preparationGeneration": generation,
                    })
                )
                .await
                .is_err()
            );
            assert!(!core.doc_host.chat_has_commands(&chat_id).unwrap());
            assert_eq!(
                core.workspace.session_startup(&chat_id).unwrap(),
                Some(preparing.clone())
            );
        }
    }
    let first_request = serde_json::json!({
        "chatId": chat_id, "commandId": "first", "command": run,
        "preparationGeneration": "new-attempt",
    });
    let (first, retry) = tokio::join!(
        rpc.handle(methods::QUEUE_COMMAND, first_request.clone()),
        rpc.handle(methods::QUEUE_COMMAND, first_request),
    );
    first.unwrap();
    retry.unwrap();
    let commands = core
        .doc_host
        .open(&chat_id)
        .unwrap()
        .doc()
        .read_commands()
        .unwrap();
    assert_eq!(
        commands
            .iter()
            .map(|command| command.id.as_str())
            .collect::<Vec<_>>(),
        ["first"]
    );
    assert!(
        core.doc_host
            .command_entry(&chat_id, "first")
            .unwrap()
            .is_some()
    );
    let admitted = core.workspace.session_startup(&chat_id).unwrap().unwrap();
    assert_eq!(admitted.status, SessionStartupStatus::Admitted);
    assert_eq!(admitted.command_id.as_deref(), Some("first"));
    rpc.handle(
        methods::QUEUE_COMMAND,
        serde_json::json!({
            "chatId": chat_id, "commandId": "followup", "command": run,
        }),
    )
    .await
    .unwrap();
    assert!(
        core.doc_host
            .command_entry(&chat_id, "followup")
            .unwrap()
            .is_some()
    );
    assert!(
        rpc.handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": chat_id, "commandId": "late-stale", "command": run,
                "preparationGeneration": "old-attempt",
            })
        )
        .await
        .is_err()
    );
    assert!(
        core.doc_host
            .command_entry(&chat_id, "late-stale")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        core.workspace.session_startup(&chat_id).unwrap(),
        Some(admitted)
    );
    core.shutdown().await;
}

#[tokio::test]
async fn failure_reports_only_mark_the_matching_existing_unadmitted_preparation() {
    use comet_proto::{SessionStartup, SessionStartupStatus};
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
    let rpc = core.rpc_service();
    for (chat_id, status, reported_generation, changes) in [
        ("matched", SessionStartupStatus::Preparing, "current", true),
        ("stale", SessionStartupStatus::Preparing, "old", false),
        ("admitted", SessionStartupStatus::Admitted, "current", false),
        (
            "uncertain",
            SessionStartupStatus::CreationUncertain,
            "current",
            false,
        ),
    ] {
        let original = SessionStartup {
            generation: "current".into(),
            status,
            updated_at: chrono::Utc::now(),
            command_id: (status == SessionStartupStatus::Admitted).then(|| "first".into()),
        };
        core.workspace
            .update_session_startup(chat_id, None, original.clone())
            .unwrap();
        rpc.handle(
            methods::REPORT_SCAFFOLD_PREPARATION_FAILURE,
            serde_json::json!({
                "chatId": chat_id, "generation": reported_generation,
            }),
        )
        .await
        .unwrap();
        let actual = core.workspace.session_startup(chat_id).unwrap().unwrap();
        if changes {
            assert_eq!(actual.status, SessionStartupStatus::AttentionNeeded);
            assert_eq!(actual.generation, original.generation);
        } else {
            assert_eq!(actual, original);
        }
    }
    core.workspace
        .upsert_session_ref("untracked", None)
        .unwrap();
    let untracked = core
        .workspace
        .doc()
        .session_ref("owner@example.com", "untracked")
        .unwrap();
    core.workspace.remove_session_ref("matched").unwrap();
    for chat_id in ["missing", "matched", "untracked"] {
        rpc.handle(
            methods::REPORT_SCAFFOLD_PREPARATION_FAILURE,
            serde_json::json!({
                "chatId": chat_id, "generation": "current",
            }),
        )
        .await
        .unwrap();
    }
    for chat_id in ["missing", "matched"] {
        assert!(
            core.workspace
                .doc()
                .session_ref("owner@example.com", chat_id)
                .unwrap()
                .is_none()
        );
        assert!(core.workspace.doc().chat(chat_id).unwrap().is_none());
    }
    assert_eq!(
        core.workspace
            .doc()
            .session_ref("owner@example.com", "untracked")
            .unwrap(),
        untracked
    );
    core.shutdown().await;
}

#[tokio::test]
async fn transport_cause_survives_rpc_boundary_without_url_or_token() {
    use crate::scaffold::ScaffoldClient;
    use tokio::io::AsyncReadExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let provider = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        let mut request = [0; 8192];
        let _ = connection.read(&mut request).await;
    });
    let client = ScaffoldClient::new(
        &origin,
        "project-a",
        std::sync::Arc::new(comet_rpc::StaticToken("bearer-secret".into())),
    )
    .unwrap();
    let error = client
        .inspect(
            "sandbox-sensitive-id",
            &params().scope,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    provider.await.unwrap();
    let error = scaffold_control_error(error);
    assert_eq!(
        error.to_string(),
        "scaffold_request_failed: connection_reset_or_closed"
    );
    assert!(!error.to_string().contains("bearer-secret"));
    assert!(!error.to_string().contains("sandbox-sensitive-id"));
}

#[tokio::test]
async fn provider_auth_text_cannot_make_uncertain_creation_retryable() {
    use comet_proto::SessionStartupStatus;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    for pre_dispatch in [true, false] {
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
                    std::sync::Arc::new(comet_rpc::StaticToken(
                        if pre_dispatch { "" } else { "test" }.into(),
                    )),
                )
                .unwrap(),
                &origin,
                std::sync::Arc::new(crate::UnavailableDeviceJoinGrantProvider),
            )
            .with_deployment_id("deployment-a".into()),
        );
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
            let body =
                r#"{"error":"scaffold_auth_unavailable","message":"scaffold_auth_unavailable"}"#;
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
            })
            .await
            .unwrap();
            result.unwrap_err()
        };
        assert!(error.to_string().contains("scaffold_auth_unavailable"));
        assert_eq!(
            matches!(error, RpcError::ScaffoldAuthUnavailable),
            pre_dispatch
        );
        let startup = core
            .workspace
            .session_startup(params().scope.session_id.as_deref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            startup.status,
            if pre_dispatch {
                SessionStartupStatus::AttentionNeeded
            } else {
                SessionStartupStatus::CreationUncertain
            }
        );
        if !pre_dispatch {
            assert!(rpc.prepare_scaffold_session(params()).await.is_err());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(10), listener.accept())
                .await
                .is_err()
        );
        core.shutdown().await;
    }
}
