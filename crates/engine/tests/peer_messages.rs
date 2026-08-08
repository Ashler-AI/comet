use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_doc::{
    MessagePart, SessionCommandEntry, SessionCommandPayload, SessionCommandStatus,
    SessionMessageEntry,
};
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, ChatConfig, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest,
    RuntimeProfile, SandboxLevel, SteeringMode,
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
    requests: RequestLog,
    run_number: AtomicU64,
}

#[async_trait]
impl Harness for RecordingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "Recording"
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.requests.lock().await.push(request.clone());
        let number = self.run_number.fetch_add(1, Ordering::Relaxed);
        let session_id = format!("peer-session-{number}");
        let events = vec![
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
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
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), "peer-test-device").expect("write device id");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let registry = HarnessRegistry::for_profile(RuntimeProfile::Mock);
    registry.register(Arc::new(RecordingHarness {
        requests: requests.clone(),
        run_number: AtomicU64::new(1),
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

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn command(core: &EngineCore, chat_id: &str, command_id: &str) -> Option<SessionCommandEntry> {
    core.doc_host
        .command_entry(chat_id, command_id)
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

fn expected_prompt(
    source_chat_id: &str,
    thread_id: &str,
    target_chat_id: &str,
    command_id: &str,
    text: &str,
) -> String {
    format!(
        "Message from Comet session {source_chat_id} (thread {thread_id}):\n\n\
         {text}\n\n\
         To reply through Comet, run:\n\
         comet session reply --session {target_chat_id} --command {command_id} \"<reply>\""
    )
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
async fn peer_message_delivers_the_visible_prompt_and_reply_uses_stored_source() {
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
    let delivered = expected_prompt(SOURCE, COMMAND, TARGET, COMMAND, "review the patch");
    wait_for(
        || {
            command(&core, TARGET, COMMAND)
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
        "the harness and transcript must receive the same visible prompt"
    );

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
        || {
            command(&core, SOURCE, reply_id)
                .is_some_and(|entry| entry.status == SessionCommandStatus::Applied)
        },
        "derived reply delivery",
    )
    .await;

    let reply_entry = command(&core, SOURCE, reply_id).expect("reply on derived source session");
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
    let reply_prompt = expected_prompt(TARGET, COMMAND, SOURCE, reply_id, "the patch is clean");
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

    core.shutdown().await;
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
        .expect("queue hop-eight command");
    let delivered = expected_prompt(SOURCE, COMMAND, TARGET, HOP_COMMAND, "final hop");
    wait_for(
        || {
            command(&core, TARGET, HOP_COMMAND)
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
        expected_prompt(SOURCE, WAIT_COMMAND, TARGET, WAIT_COMMAND, "please answer");

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
        || {
            command(&core, TARGET, WAIT_COMMAND)
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
        || {
            command(&core, SOURCE, &reply_id)
                .is_some_and(|entry| entry.status == SessionCommandStatus::Applied)
        },
        "waiter reply status",
    )
    .await;
    let reply_prompt = expected_prompt(TARGET, WAIT_COMMAND, SOURCE, &reply_id, "waiter answer");
    let transcript = entries(&core, SOURCE);
    assert_eq!(
        transcript
            .iter()
            .filter(|entry| entry.id == reply_id)
            .count(),
        1,
        "the waiter path still records one visible transcript entry"
    );
    assert_eq!(
        message_text(&core, SOURCE, &reply_id).as_deref(),
        Some(reply_prompt.as_str())
    );
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
    let target_prompt = expected_prompt(
        SOURCE,
        LATE_COMMAND,
        TARGET,
        LATE_COMMAND,
        "answer after timeout",
    );

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
    let reply_prompt = expected_prompt(TARGET, LATE_COMMAND, SOURCE, &reply_id, "late answer");
    wait_for(
        || {
            command(&core, SOURCE, &reply_id)
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

    core.shutdown().await;
}
