use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_doc::{
    MessagePart, MessageRole, MessageStatus, SessionCommandEntry, SessionCommandPayload,
    SessionCommandStatus, SessionMessageEntry,
};
use comet_engine::{EngineCore, HarnessRegistry, Repos};
use comet_harness::{Harness, HarnessError, RunControls, SteerMessage};
use comet_engine::doc_host::peer_message_prompt;
use comet_proto::{
    AgentEvent, ChatConfig, DoneStatus, HarnessId, Model, PeerMessageProvenance, ReasoningLevel,
    RunRequest, RuntimeProfile, SandboxLevel, SteeringMode,
};
use comet_rpc::methods;
use tokio::sync::Mutex;

const SOURCE: &str = "00000000-0000-4000-8000-00000000000a";
const TARGET: &str = "00000000-0000-4000-8000-00000000000b";
const COMMAND: &str = "00000000-0000-4000-8000-00000000000c";
const HOP_COMMAND: &str = "00000000-0000-4000-8000-00000000000d";
const WAIT_COMMAND: &str = "00000000-0000-4000-8000-00000000000e";
const LATE_COMMAND: &str = "00000000-0000-4000-8000-00000000000f";

type RequestLog = Arc<Mutex<Vec<RunRequest>>>;

struct RecordingHarness {
    harness: HarnessId,
    requests: RequestLog,
    run_number: AtomicU64,
    steering: Option<(SteeringMode, tokio::sync::mpsc::UnboundedSender<SteerMessage>)>,
}

#[async_trait]
impl Harness for RecordingHarness {
    fn id(&self) -> HarnessId {
        self.harness
    }

    fn display_name(&self) -> &str {
        "Recording"
    }

    fn supports_steering(&self) -> bool {
        self.steering.is_some()
    }

    fn steering_mode(&self) -> SteeringMode {
        self.steering.as_ref().map_or(SteeringMode::TurnBoundary, |(mode, _)| *mode)
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        if self.harness == HarnessId::Omp { &[ReasoningLevel::XHigh] } else { &[] }
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        if self.harness != HarnessId::Omp { return Ok(Vec::new()); }
        Ok(vec![Model {
            id: "mock-peer".into(), label: "Peer test".into(), description: None,
            reasoning_levels: self.reasoning_levels().to_vec(), options: vec![],
        }])
    }

    async fn run(
        &self,
        request: RunRequest,
        mut controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.requests.lock().await.push(request.clone());
        let number = self.run_number.fetch_add(1, Ordering::Relaxed);
        let session_id = format!("peer-session-{number}");
        if let Some((_, received)) = &self.steering {
            let received = received.clone();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let harness = self.harness;
            tokio::spawn(async move {
                let _ = tx.send(Ok(AgentEvent::SessionStarted {
                    harness,
                    model: "mock-peer".into(),
                    tools: Vec::new(),
                    cwd: request.cwd,
                    session_id: session_id.clone(),
                    assistant_message_id: format!("peer-assistant-{number}"),
                }));
                loop {
                    tokio::select! {
                        _ = controls.interrupt.cancelled() => break,
                        message = controls.steering.recv() => {
                            let Some(message) = message else { break; };
                            if received.send(message).is_err() { break; }
                        }
                    }
                }
                let _ = tx.send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(session_id),
                }));
            });
            return Ok(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            }).boxed());
        }
        let events = vec![
            Ok(AgentEvent::SessionStarted {
                harness: self.harness,
                model: "mock-peer".into(),
                tools: Vec::new(),
                cwd: request.cwd,
                session_id: session_id.clone(),
                assistant_message_id: format!("peer-assistant-{number}"),
            }),
            Ok(AgentEvent::TextDelta { text: "ack".into() }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some(session_id),
            }),
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

fn assemble(dir: &std::path::Path) -> (EngineCore, RequestLog) {
    assemble_with_steering(dir, None)
}

fn assemble_with_steering(
    dir: &std::path::Path,
    steering: Option<(SteeringMode, tokio::sync::mpsc::UnboundedSender<SteerMessage>)>,
) -> (EngineCore, RequestLog) {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), "peer-test-device").expect("write device id");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let registry = HarnessRegistry::for_profile(RuntimeProfile::Mock);
    registry.register(Arc::new(RecordingHarness {
        harness: HarnessId::Mock,
        requests: requests.clone(),
        run_number: AtomicU64::new(1),
        steering,
    }));
    let core = EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");
    (core, requests)
}

fn host_chats(core: &EngineCore, chat_ids: &[&str]) {
    core.workspace
        .create_space("peer-space", &core.device_id, "/tmp/peer", None, false)
        .expect("create peer test space");
    let config = ChatConfig {
        harness: HarnessId::Mock,
        model: None,
        reasoning: None,
        agent_account_id: None,
        model_options: Default::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
    };
    for chat_id in chat_ids {
        core.workspace
            .create_chat(chat_id, "peer-space", Some(config.clone()), None)
            .expect("create hosted chat");
        core.workspace
            .rename_chat(chat_id, &format!("Peer {chat_id}"))
            .expect("pre-title hosted chat");
    }
}

async fn wait_for<F, Fut>(mut predicate: F, what: &str)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !predicate().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn command(core: &EngineCore, chat_id: &str, command_id: &str) -> Option<SessionCommandEntry> {
    core.doc_host
        .command_entry(chat_id, command_id)
        .await
        .expect("read command entry")
}

fn entries(core: &EngineCore, chat_id: &str) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(chat_id)
        .expect("open chat")
        .doc()
        .read_entries()
        .expect("read transcript")
}

fn message_text(core: &EngineCore, chat_id: &str, message_id: &str) -> Option<String> {
    entries(core, chat_id)
        .into_iter()
        .find(|entry| entry.id == message_id)
        .and_then(|entry| {
            entry.parts.into_iter().find_map(|part| match part {
                MessagePart::Text { text, .. } => Some(text),
                _ => None,
            })
        })
}

#[tokio::test]
async fn send_auto_refs_foreign_target_and_dedupes_caller_command_id() {
    let dir = tempfile::tempdir().unwrap();
    let (core, requests) = assemble(dir.path());
    host_chats(&core, &[SOURCE]);
    let client = comet_rpc::memory_client(core.rpc_service());
    let self_send = client
        .call(
            methods::SEND_PEER_MESSAGE,
            serde_json::json!({
                "sourceChatId": SOURCE,
                "targetChatId": SOURCE.to_ascii_uppercase(),
                "text": "must not alias the same room",
            }),
        )
        .await
        .expect_err("UUID casing cannot bypass self-send rejection");
    assert_eq!(self_send.to_string(), "self_peer_message");
    let params = serde_json::json!({
        "sourceChatId": SOURCE.to_ascii_uppercase(),
        "targetChatId": TARGET.to_ascii_uppercase(),
        "text": "review the patch",
        "commandId": COMMAND,
    });

    let first = client
        .call(methods::SEND_PEER_MESSAGE, params.clone())
        .await
        .expect("first peer send");
    let second = client
        .call(methods::SEND_PEER_MESSAGE, params)
        .await
        .expect("idempotent peer send");
    assert_eq!(first, second);
    assert_eq!(first["commandId"], COMMAND);
    assert_eq!(first["threadId"], COMMAND);

    let user_id = core.auth().user_id().expect("development user id");
    let session_ref = core
        .workspace
        .doc()
        .session_ref(&user_id, TARGET)
        .expect("read target membership")
        .expect("target auto-ref");
    assert_eq!(session_ref.chat_id, TARGET);
    assert!(
        core.workspace.doc().chat(TARGET).unwrap().is_none(),
        "sending to a foreign session must not create a host row"
    );
    assert!(!core.workspace.is_host(TARGET));

    let handle = core.doc_host.open(TARGET).expect("target was opened");
    let commands = handle.doc().read_commands().expect("read target commands");
    assert_eq!(commands.len(), 1, "caller command id must dedupe appends");
    assert_eq!(commands[0].id, COMMAND);
    assert_eq!(commands[0].status, SessionCommandStatus::Pending);
    assert!(matches!(
        &commands[0].payload,
        SessionCommandPayload::PeerMessage {
            text,
            source_chat_id,
            thread_id,
            reply_to: None,
            hop_count: 0,
        } if text == "review the patch" && source_chat_id == SOURCE && thread_id == COMMAND
    ));
    assert!(
        requests.lock().await.is_empty(),
        "an importer is not the target host"
    );

    core.shutdown().await;
}

#[tokio::test]
async fn peer_message_provenance_preserves_delivery_reply_correlation_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (core, requests) = assemble(dir.path());
    host_chats(&core, &[SOURCE, TARGET]);
    let client = comet_rpc::memory_client(core.rpc_service());

    client
        .call(
            methods::SEND_PEER_MESSAGE,
            serde_json::json!({
                "sourceChatId": SOURCE,
                "targetChatId": TARGET,
                "text": "review the patch",
                "commandId": COMMAND,
            }),
        )
        .await
        .expect("send peer message");
    let delivered = peer_message_prompt(SOURCE, COMMAND, TARGET, COMMAND, "review the patch");
    wait_for(
        || async {
            command(&core, TARGET, COMMAND)
                .await
                .is_some_and(|entry| entry.status == SessionCommandStatus::Applied)
                && message_text(&core, TARGET, COMMAND).as_deref() == Some(delivered.as_str())
                && requests
                    .try_lock()
                    .is_ok_and(|logged| logged.iter().any(|request| request.prompt == delivered))
        },
        "target prompt delivery",
    )
    .await;
    assert!(
        requests
            .lock()
            .await
            .iter()
            .any(|request| request.prompt == delivered),
        "the harness and transcript must receive the same original prompt"
    );
    let target_entry = entries(&core, TARGET).into_iter().find(|e| e.id == COMMAND).unwrap();
    assert!(target_entry.is_peer_message());
    assert_eq!(target_entry.peer_message, Some(PeerMessageProvenance {
        command_id: COMMAND.into(),
        source_chat_id: SOURCE.into(),
        thread_id: COMMAND.into(),
        reply_to: None,
    }));

    let reply = client
        .call(
            methods::REPLY_PEER_MESSAGE,
            serde_json::json!({
                "sessionId": TARGET,
                "commandId": COMMAND,
                "text": "the patch is clean",
            }),
        )
        .await
        .expect("reply using stored peer command");
    let reply_id = reply["commandId"].as_str().expect("reply command id");
    assert_eq!(reply["threadId"], COMMAND);
    wait_for(
        || async {
            command(&core, SOURCE, reply_id)
                .await
                .is_some_and(|entry| entry.status == SessionCommandStatus::Applied)
        },
        "derived reply delivery",
    )
    .await;

    let reply_entry = command(&core, SOURCE, reply_id).await.expect("reply on derived source session");
    assert!(matches!(
        &reply_entry.payload,
        SessionCommandPayload::PeerMessage {
            text,
            source_chat_id,
            thread_id,
            reply_to: Some(reply_to),
            hop_count: 1,
        } if text == "the patch is clean"
            && source_chat_id == TARGET
            && thread_id == COMMAND
            && reply_to == COMMAND
    ));
    let reply_prompt = peer_message_prompt(TARGET, COMMAND, SOURCE, reply_id, "the patch is clean");
    assert_eq!(
        message_text(&core, SOURCE, reply_id).as_deref(),
        Some(reply_prompt.as_str())
    );
    assert!(
        requests
            .lock()
            .await
            .iter()
            .any(|request| request.prompt == reply_prompt)
    );
    let source_entry = entries(&core, SOURCE).into_iter().find(|e| e.id == reply_id).unwrap();
    assert!(source_entry.is_peer_message());
    assert_eq!(source_entry.peer_message, Some(PeerMessageProvenance {
        command_id: reply_id.into(),
        source_chat_id: TARGET.into(),
        thread_id: COMMAND.into(),
        reply_to: Some(COMMAND.into()),
    }));

    core.shutdown().await;
    drop(client);
    drop(core);
    let (restarted, restart_requests) = assemble(dir.path());
    for (chat_id, expected) in [(TARGET, target_entry), (SOURCE, source_entry)] {
        let restored = entries(&restarted, chat_id).into_iter().find(|e| e.id == expected.id).unwrap();
        assert_eq!(restored, expected);
        assert!(restored.is_peer_message());
        assert_eq!(command(&restarted, chat_id, &restored.id).await.unwrap().status, SessionCommandStatus::Applied);
    }
    assert!(restart_requests.lock().await.is_empty(), "restart must not redeliver settled peer commands");
    restarted.shutdown().await;
}

#[tokio::test]
async fn peer_reply_rejects_a_delivered_hop_eight_command() {
    let dir = tempfile::tempdir().unwrap();
    let (core, requests) = assemble(dir.path());
    host_chats(&core, &[SOURCE, TARGET]);
    core.doc_host
        .queue_command_with_id(
            TARGET,
            HOP_COMMAND,
            SessionCommandPayload::PeerMessage {
                text: "final hop".into(),
                source_chat_id: SOURCE.into(),
                thread_id: COMMAND.into(),
                reply_to: Some(COMMAND.into()),
                hop_count: 8,
            },
        )
        .await
        .expect("queue hop-eight command");
    let delivered = peer_message_prompt(SOURCE, COMMAND, TARGET, HOP_COMMAND, "final hop");
    wait_for(
        || async {
            command(&core, TARGET, HOP_COMMAND)
                .await
                .is_some_and(|entry| entry.status == SessionCommandStatus::Applied)
                && message_text(&core, TARGET, HOP_COMMAND).as_deref() == Some(delivered.as_str())
                && requests
                    .try_lock()
                    .is_ok_and(|logged| logged.iter().any(|request| request.prompt == delivered))
        },
        "hop-eight delivery",
    )
    .await;
    assert!(
        requests
            .lock()
            .await
            .iter()
            .any(|request| request.prompt == delivered),
        "hop eight is delivered even though another reply is forbidden"
    );

    let client = comet_rpc::memory_client(core.rpc_service());
    let error = client
        .call(
            methods::REPLY_PEER_MESSAGE,
            serde_json::json!({
                "sessionId": TARGET,
                "commandId": HOP_COMMAND,
                "text": "hop nine must fail",
            }),
        )
        .await
        .expect_err("hop eight cannot be replied to");
    assert_eq!(error.to_string(), "peer_hop_limit");
    assert!(
        core.doc_host
            .open(SOURCE)
            .expect("open source")
            .doc()
            .read_commands()
            .unwrap()
            .is_empty(),
        "rejection must not append a hop-nine command"
    );

    core.shutdown().await;
}

#[tokio::test]
async fn live_waiter_returns_reply_without_double_delivering_to_harness() {
    let dir = tempfile::tempdir().unwrap();
    let (core, requests) = assemble(dir.path());
    host_chats(&core, &[SOURCE, TARGET]);
    let send_client = comet_rpc::memory_client(core.rpc_service());
    let reply_client = comet_rpc::memory_client(core.rpc_service());
    let target_prompt =
        peer_message_prompt(SOURCE, WAIT_COMMAND, TARGET, WAIT_COMMAND, "please answer");

    let send = tokio::spawn(async move {
        send_client
            .call(
                methods::SEND_PEER_MESSAGE,
                serde_json::json!({
                    "sourceChatId": SOURCE,
                    "targetChatId": TARGET,
                    "text": "please answer",
                    "commandId": WAIT_COMMAND,
                    "wait": true,
                    "timeoutMs": 2_000,
                }),
            )
            .await
    });
    wait_for(
        || async {
            command(&core, TARGET, WAIT_COMMAND)
                .await
                .is_some_and(|entry| entry.status == SessionCommandStatus::Applied)
                && requests.try_lock().is_ok_and(|logged| {
                    logged.iter().any(|request| request.prompt == target_prompt)
                })
        },
        "waiting peer command delivery",
    )
    .await;

    let queued_reply = reply_client
        .call(
            methods::REPLY_PEER_MESSAGE,
            serde_json::json!({
                "sessionId": TARGET,
                "commandId": WAIT_COMMAND,
                "text": "waiter answer",
            }),
        )
        .await
        .expect("queue waiter reply");
    let reply_id = queued_reply["commandId"]
        .as_str()
        .expect("waiter reply command id")
        .to_owned();
    let send_result = tokio::time::timeout(Duration::from_secs(2), send)
        .await
        .expect("wait RPC completes")
        .expect("send task joins")
        .expect("send RPC succeeds");
    assert_eq!(send_result["reply"]["commandId"], reply_id);
    assert_eq!(send_result["reply"]["text"], "waiter answer");
    assert_eq!(send_result["reply"]["sourceChatId"], TARGET);

    wait_for(
        || async {
            command(&core, SOURCE, &reply_id)
                .await
                .is_some_and(|entry| entry.status == SessionCommandStatus::Applied)
        },
        "waiter reply status",
    )
    .await;
    let reply_prompt = peer_message_prompt(TARGET, WAIT_COMMAND, SOURCE, &reply_id, "waiter answer");
    let transcript = entries(&core, SOURCE);
    assert_eq!(
        transcript
            .iter()
            .filter(|entry| entry.id == reply_id)
            .count(),
        1,
        "the waiter path still records exactly one inspectable transcript entry"
    );
    assert_eq!(
        message_text(&core, SOURCE, &reply_id).as_deref(),
        Some(reply_prompt.as_str())
    );
    let peer = transcript.iter().find(|entry| entry.id == reply_id).unwrap();
    assert!(peer.is_peer_message());
    assert_eq!(peer.peer_message.as_ref().unwrap().reply_to.as_deref(), Some(WAIT_COMMAND));
    {
        let logged = requests.lock().await;
        assert_eq!(
            logged.len(),
            1,
            "the reply must not also dispatch as a new harness turn"
        );
        assert_eq!(logged[0].prompt, target_prompt);
    }

    core.shutdown().await;
}

#[tokio::test]
async fn timed_out_waiter_allows_a_late_reply_to_deliver_normally() {
    let dir = tempfile::tempdir().unwrap();
    let (core, requests) = assemble(dir.path());
    host_chats(&core, &[SOURCE, TARGET]);
    let client = comet_rpc::memory_client(core.rpc_service());
    let target_prompt = peer_message_prompt(SOURCE,
    LATE_COMMAND,
    TARGET,
    LATE_COMMAND,
    "answer after timeout",);

    let timed_out = client
        .call(
            methods::SEND_PEER_MESSAGE,
            serde_json::json!({
                "sourceChatId": SOURCE,
                "targetChatId": TARGET,
                "text": "answer after timeout",
                "commandId": LATE_COMMAND,
                "wait": true,
                "timeoutMs": 20,
            }),
        )
        .await
        .expect("send returns after waiter timeout");
    assert!(
        timed_out.get("reply").is_none(),
        "a timeout has no synchronous reply"
    );

    let reply = client
        .call(
            methods::REPLY_PEER_MESSAGE,
            serde_json::json!({
                "sessionId": TARGET,
                "commandId": LATE_COMMAND,
                "text": "late answer",
            }),
        )
        .await
        .expect("queue late reply");
    let reply_id = reply["commandId"]
        .as_str()
        .expect("late reply command id")
        .to_owned();
    let reply_prompt = peer_message_prompt(TARGET, LATE_COMMAND, SOURCE, &reply_id, "late answer");
    wait_for(
        || async {
            command(&core, SOURCE, &reply_id)
                .await
                .is_some_and(|entry| entry.status == SessionCommandStatus::Applied)
                && requests.try_lock().is_ok_and(|logged| {
                    logged.iter().any(|request| request.prompt == reply_prompt)
                        && logged.iter().any(|request| request.prompt == target_prompt)
                })
        },
        "late reply normal delivery",
    )
    .await;
    assert_eq!(
        message_text(&core, SOURCE, &reply_id).as_deref(),
        Some(reply_prompt.as_str())
    );
    assert_eq!(
        requests.lock().await.len(),
        2,
        "target delivery plus the post-timeout source delivery"
    );
    assert!(entries(&core, SOURCE).iter().find(|entry| entry.id == reply_id).unwrap().is_peer_message());

    core.shutdown().await;
}

#[tokio::test]
async fn peer_visibility_preserves_active_steering_and_turn_boundary_delivery() {
    for (mode, expected_status) in [
        (SteeringMode::StepBoundary, MessageStatus::Steered),
        (SteeringMode::TurnBoundary, MessageStatus::Queued),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (received, mut steering) = tokio::sync::mpsc::unbounded_channel();
        let (core, requests) = assemble_with_steering(dir.path(), Some((mode, received)));
        host_chats(&core, &[SOURCE, TARGET]);
        let lookalike = peer_message_prompt(SOURCE, COMMAND, TARGET, COMMAND, "ordinary typed message");
        core.sessions.dispatch(TARGET, HarnessId::Mock, RunRequest {
            prompt: lookalike.clone(),
            model: None,
            agent_account_id: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "/tmp/peer".into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            attachments: Vec::new(),
            resume: None,
        }, Some("ordinary-user".into())).await.unwrap();
        let ordinary = entries(&core, TARGET).into_iter().find(|e| e.id == "ordinary-user").unwrap();
        assert!(!ordinary.is_peer_message());
        assert!(ordinary.peer_message.is_none());
        assert_eq!(message_text(&core, TARGET, "ordinary-user"), Some(lookalike.clone()));
        assert_eq!(requests.lock().await[0].prompt, lookalike);

        let client = comet_rpc::memory_client(core.rpc_service());
        client.call(methods::SEND_PEER_MESSAGE, serde_json::json!({
            "sourceChatId": SOURCE,
            "targetChatId": TARGET,
            "text": "private-peer-body",
            "commandId": COMMAND,
        })).await.unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(5), steering.recv())
            .await.unwrap().unwrap();
        wait_for(|| async { command(&core, TARGET, COMMAND).await
            .is_some_and(|entry| entry.status == SessionCommandStatus::Applied) }, "active peer delivery").await;
        let peer = entries(&core, TARGET).into_iter().find(|e| e.id == COMMAND).unwrap();
        assert!(peer.is_peer_message());
        assert_eq!(peer.status, Some(expected_status));
        assert_eq!(delivered.message_id.as_deref(), Some(COMMAND));
        assert_eq!(Some(delivered.prompt), message_text(&core, TARGET, COMMAND));
        assert_eq!(requests.lock().await.len(), 1, "steering must not dispatch a replacement run");
        let chat = core.workspace.doc().chat(TARGET).unwrap().unwrap();
        let preview = chat.last_message_preview.unwrap();
        assert!(!preview.contains("private-peer-body"));
        assert!(chat.last_message_at.is_some(), "peer activity freshness remains intact");
        // A later large output can consume the bounded window's entire budget.
        // Explicit reveal must still retrieve the unchanged original peer prompt.
        core.doc_host.open(TARGET).unwrap().doc().push_message(&SessionMessageEntry {
            id: "large-output".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text { id: "t0".into(), text: "x".repeat(comet_doc::TAIL_TEXT_BYTE_BUDGET) }],
            created_at: 2,
            device_id: core.device_id.clone(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            peer_message: None,
        }).unwrap();
        let handle = core.doc_host.open(TARGET).unwrap();
        let window = handle.doc().read_entry_window(None, 64).unwrap();
        let projected = window.entries.iter().find(|entry| entry.id == COMMAND).unwrap();
        assert!(projected.parts.iter().any(|part| matches!(part, MessagePart::TextWindow { .. })));
        let original: SessionMessageEntry = serde_json::from_value(client.call(
            methods::READ_DOC_MESSAGE,
            serde_json::json!({ "chatId": TARGET, "messageId": COMMAND }),
        ).await.unwrap()).unwrap();
        assert_eq!(original, peer);
        assert!(client.call(methods::READ_DOC_MESSAGE,
            serde_json::json!({ "chatId": TARGET, "messageId": "missing" }),
        ).await.is_err());
        core.shutdown().await;
    }
}

#[tokio::test]
async fn historical_peer_command_identity_never_retrofits_unmarked_messages() {
    let dir = tempfile::tempdir().unwrap();
    let (core, _) = assemble(dir.path());
    host_chats(&core, &[SOURCE, TARGET]);
    let handle = core.doc_host.open(TARGET).unwrap();
    let prompt = peer_message_prompt(SOURCE, COMMAND, TARGET, COMMAND, "retained historical context");
    let original = SessionMessageEntry {
        id: COMMAND.into(),
        role: MessageRole::User,
        parts: vec![MessagePart::Text { id: "t0".into(), text: prompt.clone() }],
        created_at: 1,
        device_id: core.device_id.clone(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
        peer_message: None,
    };
    handle.doc().push_message(&original).unwrap();
    handle.doc().queue_command(&SessionCommandEntry {
        id: COMMAND.into(),
        payload: SessionCommandPayload::PeerMessage {
            text: "retained historical context".into(),
            source_chat_id: SOURCE.into(),
            thread_id: COMMAND.into(),
            reply_to: None,
            hop_count: 0,
        },
        issued_by: core.device_id.clone(),
        issued_at: 1,
        based_on: None,
        expires_at: None,
        status: SessionCommandStatus::Applied,
        resolution: None,
    }).unwrap();
    assert!(!handle.write_user_message(COMMAND, &prompt, 2).unwrap());
    assert_eq!(entries(&core, TARGET), vec![original]);
    core.shutdown().await;
}

const WORKER_OWNER: &str = "00000000-0000-4000-8000-000000000010";
const OUTSIDER: &str = "00000000-0000-4000-8000-000000000011";

fn worker_engine(root: &std::path::Path) -> (EngineCore, RequestLog) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(RecordingHarness {
        harness: HarnessId::Omp,
        requests: requests.clone(), run_number: AtomicU64::new(1), steering: None,
    }));
    let data = root.join("data");
    let mut core = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Omp, None).unwrap();
    core.repos = Repos::with_worktrees_root(&data, &core.device_id, root.join("worktrees"));
    (core, requests)
}

async fn nested_workers(root: &std::path::Path) -> (EngineCore, RequestLog) {
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    for args in [vec!["init", "-b", "main"], vec!["-c", "user.name=Test", "-c",
        "user.email=test@example.invalid", "commit", "--allow-empty", "-m", "base"]] {
        let output = std::process::Command::new("git").args(args).current_dir(&project)
            .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    let project = std::fs::canonicalize(project).unwrap();
    let (core, requests) = worker_engine(root);
    core.workspace.create_space("owner-space", &core.device_id, project.to_str().unwrap(), None, true).unwrap();
    for id in [WORKER_OWNER, OUTSIDER] {
        core.workspace.create_chat(id, "owner-space", None, None).unwrap();
    }
    let client = comet_rpc::memory_client(core.rpc_service());
    for (worker, owner) in [(SOURCE, WORKER_OWNER), (TARGET, SOURCE)] {
        client.call(methods::ENSURE_WORKER_SESSION, serde_json::json!({
            "chatId": worker, "ownerChatId": owner, "projectPath": project,
            "baseRef": "main", "title": "nested peer", "model": "mock-peer", "effort": "xhigh",
        })).await.unwrap();
    }
    (core, requests)
}

#[tokio::test]
async fn child_reply_reaches_worker_waiter_and_survives_durable_reconnect() {
    let root = tempfile::tempdir().unwrap();
    let (core, requests) = nested_workers(root.path()).await;
    let client = comet_rpc::memory_client(core.rpc_service());
    let sender = comet_rpc::memory_client(core.rpc_service());
    let waiting = tokio::spawn(async move {
        sender.call(methods::SEND_PEER_MESSAGE, serde_json::json!({
            "sourceChatId": SOURCE, "targetChatId": TARGET, "commandId": WAIT_COMMAND,
            "text": "answer parent", "wait": true, "timeoutMs": 4000,
        })).await.unwrap()
    });
    wait_for(|| async { command(&core, TARGET, WAIT_COMMAND).await.is_some_and(|c| c.status == SessionCommandStatus::Applied) }, "child applied request").await;
    let reply = client.call(methods::REPLY_PEER_MESSAGE, serde_json::json!({
        "sessionId": TARGET, "commandId": WAIT_COMMAND, "text": "child answer",
    })).await.unwrap();
    let result = waiting.await.unwrap();
    let reply_id = format!("reply:{WAIT_COMMAND}");
    assert_eq!(result["reply"]["commandId"], reply_id);
    assert_eq!(result["reply"]["sourceChatId"], TARGET);
    assert_eq!(result["reply"]["text"], "child answer");
    assert_eq!(reply["commandId"], reply_id);
    wait_for(|| async { command(&core, SOURCE, &reply_id).await.is_some_and(|c| c.status == SessionCommandStatus::Applied) }, "parent waiter reply").await;
    assert_eq!(requests.lock().await.len(), 1, "live reply must not dispatch another turn");
    assert_eq!(entries(&core, SOURCE).iter().find(|e| e.id == reply_id).unwrap().peer_message.as_ref().unwrap().reply_to.as_deref(), Some(WAIT_COMMAND));
    client.call(methods::SEND_PEER_MESSAGE, serde_json::json!({
        "sourceChatId": SOURCE, "targetChatId": TARGET, "commandId": LATE_COMMAND, "text": "answer after restart",
    })).await.unwrap();
    wait_for(|| async { command(&core, TARGET, LATE_COMMAND).await.is_some_and(|c| c.status == SessionCommandStatus::Applied) }, "late request applied").await;
    core.shutdown().await;
    drop(client);
    drop(core);

    let (core, requests) = worker_engine(root.path());
    let client = comet_rpc::memory_client(core.rpc_service());
    let params = serde_json::json!({"sessionId": TARGET, "commandId": LATE_COMMAND, "text": "durable child answer"});
    let reply = client.call(methods::REPLY_PEER_MESSAGE, params.clone()).await.unwrap();
    assert_eq!(client.call(methods::REPLY_PEER_MESSAGE, params).await.unwrap(), reply);
    let reply_id = format!("reply:{LATE_COMMAND}");
    wait_for(|| async { command(&core, SOURCE, &reply_id).await.is_some_and(|c| c.status == SessionCommandStatus::Applied) }, "late child reply delivered").await;
    let expected = peer_message_prompt(TARGET, LATE_COMMAND, SOURCE, &reply_id, "durable child answer");
    assert_eq!(message_text(&core, SOURCE, &reply_id).as_deref(), Some(expected.as_str()));
    assert_eq!(requests.lock().await.iter().filter(|r| r.prompt == expected).count(), 1);
    assert!(client.call(methods::REPLY_PEER_MESSAGE, serde_json::json!({
        "sessionId": TARGET, "commandId": LATE_COMMAND, "text": "changed retry",
    })).await.is_err());
    core.shutdown().await;
    drop(client);
    drop(core);
    let (core, requests) = worker_engine(root.path());
    assert_eq!(message_text(&core, SOURCE, &reply_id).as_deref(), Some(expected.as_str()));
    assert_eq!(command(&core, SOURCE, &reply_id).await.unwrap().status, SessionCommandStatus::Applied);
    assert!(requests.lock().await.is_empty());
    core.shutdown().await;
}

#[tokio::test]
async fn worker_child_reply_rejects_forgery_unrelated_threads_and_stale_ownership() {
    let root = tempfile::tempdir().unwrap();
    let (core, requests) = nested_workers(root.path()).await;
    let client = comet_rpc::memory_client(core.rpc_service());
    for source in [TARGET, OUTSIDER] {
        assert!(client.call(methods::SEND_PEER_MESSAGE, serde_json::json!({
            "sourceChatId": source, "targetChatId": SOURCE, "commandId": COMMAND, "text": "unsolicited",
        })).await.is_err());
    }
    assert!(command(&core, SOURCE, COMMAND).await.is_none());
    client.call(methods::SEND_PEER_MESSAGE, serde_json::json!({
        "sourceChatId": SOURCE, "targetChatId": TARGET, "commandId": COMMAND, "text": "legitimate request",
    })).await.unwrap();
    wait_for(|| async { command(&core, TARGET, COMMAND).await.is_some_and(|c| c.status == SessionCommandStatus::Applied) }, "original delivered").await;
    let reply_id = format!("reply:{COMMAND}");
    let reply = SessionCommandPayload::PeerMessage {
        source_chat_id: TARGET.into(), thread_id: COMMAND.into(), reply_to: Some(COMMAND.into()),
        hop_count: 1, text: "answer".into(),
    };
    assert!(core.doc_host.queue_command_with_id(OUTSIDER, "worker-peer-authority/v1/forged", reply.clone()).await.is_err());
    let original = command(&core, TARGET, COMMAND).await.unwrap();
    let child_doc = core.doc_host.open(TARGET).unwrap();
    let Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) =
        child_doc.doc().doc().get_list("commands").get(0) else { panic!("original command row"); };
    row.insert("issuedAt", original.issued_at + 1).unwrap();
    assert!(client.call(methods::REPLY_PEER_MESSAGE, serde_json::json!({
        "sessionId": TARGET, "commandId": COMMAND, "text": "same id with forged immutable metadata",
    })).await.is_err());
    row.insert("issuedAt", original.issued_at).unwrap();
    for (id, original, thread, hop) in [
        ("arbitrary-reply-id", COMMAND, COMMAND, 1),
        ("reply:missing", "missing", COMMAND, 1),
        (reply_id.as_str(), COMMAND, "wrong-thread", 1),
        (reply_id.as_str(), COMMAND, COMMAND, 2),
        (reply_id.as_str(), COMMAND, COMMAND, 9),
    ] {
        assert!(core.doc_host.queue_command_with_id(SOURCE, id, SessionCommandPayload::PeerMessage {
            source_chat_id: TARGET.into(), thread_id: thread.into(), reply_to: Some(original.into()),
            hop_count: hop, text: "forged answer".into(),
        }).await.is_err());
        assert!(command(&core, SOURCE, id).await.is_none());
    }
    let mut forged = command(&core, TARGET, COMMAND).await.unwrap();
    forged.id = HOP_COMMAND.into();
    forged.payload = SessionCommandPayload::PeerMessage {
        source_chat_id: SOURCE.into(), thread_id: HOP_COMMAND.into(), reply_to: None, hop_count: 0, text: "synced forgery".into(),
    };
    core.doc_host.open(TARGET).unwrap().doc().queue_command(&forged).unwrap();
    assert!(client.call(methods::REPLY_PEER_MESSAGE, serde_json::json!({
        "sessionId": TARGET, "commandId": HOP_COMMAND, "text": "forged original must not authorize",
    })).await.is_err());
    assert!(command(&core, SOURCE, &format!("reply:{HOP_COMMAND}")).await.is_none());
    // Synced commands can pass ordinary shared-chat membership, but neither an
    // owner string nor a correlated reply may bypass worker-local admission.
    for (id, source, reply_to, hop_count) in [
        (LATE_COMMAND, WORKER_OWNER, None, 0),
        ("reply:bypass", TARGET, Some(COMMAND), 1),
        ("non-owner-bypass", OUTSIDER, None, 0),
    ] {
        let injected = SessionCommandEntry {
            id: id.into(), payload: SessionCommandPayload::PeerMessage {
                source_chat_id: source.into(), thread_id: COMMAND.into(),
                reply_to: reply_to.map(str::to_string), hop_count, text: "synced bypass".into(),
            }, issued_by: "synced-device".into(), issued_at: original.issued_at,
            based_on: None, expires_at: None, status: SessionCommandStatus::Pending, resolution: None,
        };
        let parent = core.doc_host.open(SOURCE).unwrap();
        parent.doc().queue_command(&injected).unwrap();
        core.doc_host.drain_commands(&parent).await;
        assert_eq!(command(&core, SOURCE, id).await.unwrap().status, SessionCommandStatus::Rejected);
        assert!(message_text(&core, SOURCE, id).is_none());
    }

    let child = core.workspace.doc().worker_binding(TARGET).unwrap().unwrap();
    let mut changed = child.clone();
    changed.owner_chat_id = OUTSIDER.into();
    core.workspace.doc().set_worker_binding(&changed).unwrap();
    assert!(core.doc_host.queue_command_with_id(SOURCE, &reply_id, reply.clone()).await.is_err());
    changed = child.clone();
    changed.owner_device_id = "other-device".into();
    core.workspace.doc().set_worker_binding(&changed).unwrap();
    assert!(core.doc_host.queue_command_with_id(SOURCE, &reply_id, reply.clone()).await.is_err());
    core.workspace.doc().set_worker_binding(&child).unwrap();

    // Admission succeeded, then the binding changed before the durable queue
    // drained. Execution must recheck rather than trusting queue-time routing.
    core.doc_host.queue_command_with_id(SOURCE, &reply_id, reply).await.unwrap();
    changed = child;
    changed.owner_chat_id = OUTSIDER.into();
    core.workspace.doc().set_worker_binding(&changed).unwrap();
    core.doc_host.drain_commands(&core.doc_host.open(SOURCE).unwrap()).await;
    assert_eq!(command(&core, SOURCE, &reply_id).await.unwrap().status, SessionCommandStatus::Rejected);
    assert!(message_text(&core, SOURCE, &reply_id).is_none());
    assert_eq!(requests.lock().await.len(), 1, "rejected peers must never reach a harness");
    core.shutdown().await;
}
