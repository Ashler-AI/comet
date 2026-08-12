//! M4a integration: two `EngineCore`s (distinct data dirs + device ids) sharing one
//! per-org workspace doc.
//!
//! The in-memory bridge below stands in for the edge room: it cross-imports Loro
//! updates (`export(updates)`) between the two engines' workspace docs on a timer,
//! which is exactly what `RoomClient` + the SessionRoom DO do over the wire. A live
//! variant against a real edge runs behind `#[ignore]` (COMET_EDGE_WS, like
//! comet-sync's edge_convergence test).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_doc::{SessionCommandPayload, SessionCommandStatus};
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, ChatConfig, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SessionStatus, SteeringMode,
};
use comet_rpc::methods;

/// Scripted harness: emits SessionStarted + text + Done with a per-event delay (so
/// `Working` is observable across the bridge).
struct ScriptedHarness {
    id: HarnessId,
    text: &'static str,
    step_delay: Duration,
}

#[async_trait]
impl Harness for ScriptedHarness {
    fn id(&self) -> HarnessId {
        self.id
    }
    fn display_name(&self) -> &str {
        "Scripted"
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
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
        let harness = self.id;
        let text = self.text;
        let delay = self.step_delay;
        tokio::spawn(async move {
            let script = vec![
                AgentEvent::SessionStarted {
                    harness,
                    model: "scripted-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-1".into(),
                    assistant_message_id: "a-1".into(),
                },
                AgentEvent::TextDelta { text: text.into() },
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: Some("hs-1".into()),
                },
            ];
            for event in script {
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
                tokio::time::sleep(delay).await;
            }
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

fn registry() -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(ScriptedHarness {
        id: HarnessId::ClaudeCode,
        text: "Hello",
        step_delay: Duration::from_millis(60),
    }));
    registry.register(Arc::new(ScriptedHarness {
        id: HarnessId::Codex,
        text: "From codex",
        step_delay: Duration::from_millis(10),
    }));
    Arc::new(registry)
}

/// Assemble an engine with a fixed device id under its own data dir (offline).
fn assemble(dir: &std::path::Path, device_id: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    EngineCore::assemble(dir, registry(), HarnessId::ClaudeCode, None)
        .expect("engine core assembles")
}

/// The in-memory room: cross-import workspace-doc updates between two engines on a
/// timer (what RoomClient + the DO relay do over the wire).
fn bridge(a: &EngineCore, b: &EngineCore) -> tokio::task::JoinHandle<()> {
    let da = a.workspace.doc_arc();
    let db = b.workspace.doc_arc();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        loop {
            tick.tick().await;
            for (from, to) in [(da.doc(), db.doc()), (db.doc(), da.doc())] {
                if let Ok(update) = from.export(loro::ExportMode::updates(&to.oplog_vv()))
                    && !update.is_empty()
                {
                    let _ = to.import(&update);
                }
            }
        }
    })
}

/// Cross-import one session document between two engines, mirroring the
/// authenticated SessionRoom relay used by imported sessions.
fn bridge_session(a: &EngineCore, b: &EngineCore, chat_id: &str) -> tokio::task::JoinHandle<()> {
    let a_handle = a.doc_host.open(chat_id).expect("open session on A");
    let b_handle = b.doc_host.open(chat_id).expect("open session on B");
    let da = a_handle.doc().doc().clone();
    let db = b_handle.doc().doc().clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        loop {
            tick.tick().await;
            for (from, to) in [(&da, &db), (&db, &da)] {
                if let Ok(update) = from.export(loro::ExportMode::updates(&to.oplog_vv()))
                    && !update.is_empty()
                {
                    let _ = to.import(&update);
                }
            }
        }
    })
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn run_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: None,
        agent_account_id: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

/// Queue a run through the device-local API. This persists local provenance;
/// remotely authored commands use the grant-bearing `Control::Start` path.
fn queue_run(core: &EngineCore, chat_id: &str, message_id: &str) {
    core.doc_host
        .queue_command(
            chat_id,
            SessionCommandPayload::Run {
                request: run_request("go do it"),
                message_id: message_id.into(),
            },
        )
        .expect("queue command");
}

#[tokio::test]
async fn two_engines_share_a_workspace() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = assemble(dir_a.path(), "dev-a");
    let b = assemble(dir_b.path(), "dev-b");
    let link = bridge(&a, &b);

    // Device rows from BOTH engines appear on both sides.
    for core in [&a, &b] {
        wait_for(
            || {
                let ids: Vec<String> = core
                    .workspace
                    .doc()
                    .read_devices()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|d| d.id)
                    .collect();
                ids == ["dev-a", "dev-b"]
            },
            "both device rows",
        )
        .await;
    }

    // CreateSpace + CreateChat on A (Mutate over the real RPC surface), hosted
    // by dev-a via the space.
    let client_a = comet_rpc::memory_client(a.rpc_service());
    let client_b = comet_rpc::memory_client(b.rpc_service());
    client_a
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createSpace", "spaceId": "space-1", "deviceId": "dev-a", "path": "/tmp"
            }),
        )
        .await
        .expect("create space");
    client_a
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat", "chatId": "chat-1", "spaceId": "space-1"
            }),
        )
        .await
        .expect("create chat");
    // The space row crosses to B alongside the chat row.
    wait_for(
        || {
            b.workspace
                .read_spaces()
                .unwrap_or_default()
                .iter()
                .any(|s| s.id == "space-1" && s.device_id == "dev-a" && s.path == "/tmp")
        },
        "space row on B",
    )
    .await;
    wait_for(
        || b.workspace.doc().chat("chat-1").ok().flatten().is_some(),
        "chat row on B",
    )
    .await;

    // Run on A: B's workspace view shows the session Working, then Idle.
    queue_run(&a, "chat-1", "m-1");
    let b_status = |wanted: SessionStatus| {
        let doc = b.workspace.doc_arc();
        move || {
            doc.read_sessions()
                .unwrap_or_default()
                .iter()
                .any(|s| s.chat_id == "chat-1" && s.device_id == "dev-a" && s.status == wanted)
        }
    };
    wait_for(b_status(SessionStatus::Working), "Working on B").await;
    wait_for(b_status(SessionStatus::Idle), "Idle on B").await;

    // Sidebar freshness crossed too: the chat row's preview settles on the
    // assistant's final text (first-120-chars policy).
    wait_for(
        || {
            b.workspace
                .doc()
                .chat("chat-1")
                .ok()
                .flatten()
                .and_then(|c| c.last_message_preview)
                .as_deref()
                == Some("Hello")
        },
        "assistant preview on B",
    )
    .await;

    // Rename + archive from B (LWW from any device) become visible on A.
    client_b
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "renameChat", "chatId": "chat-1", "title": "Renamed from B" }),
        )
        .await
        .expect("rename chat");
    client_b
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "setChatArchived", "chatId": "chat-1", "archived": true }),
        )
        .await
        .expect("archive chat");
    wait_for(
        || {
            a.workspace
                .doc()
                .chat("chat-1")
                .ok()
                .flatten()
                .is_some_and(|c| c.title.as_deref() == Some("Renamed from B") && c.archived)
        },
        "rename + archive on A",
    )
    .await;

    // Device rename from B visible on A.
    client_b
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "renameDevice", "deviceId": "dev-b", "name": "B's VPS" }),
        )
        .await
        .expect("rename device");
    wait_for(
        || {
            a.workspace
                .doc()
                .read_devices()
                .unwrap_or_default()
                .iter()
                .any(|d| d.id == "dev-b" && d.name == "B's VPS")
        },
        "device rename on A",
    )
    .await;

    link.abort();
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn imported_session_chat_executes_on_its_remote_host() {
    const TARGET: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = assemble(dir_a.path(), "dev-a");
    let b = assemble(dir_b.path(), "dev-b");
    let workspace_link = bridge(&a, &b);
    let client_a = comet_rpc::memory_client(a.rpc_service());
    let client_b = comet_rpc::memory_client(b.rpc_service());

    client_b
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createSpace", "spaceId": "space-b", "deviceId": "dev-b", "path": "/tmp"
            }),
        )
        .await
        .expect("create target space");
    client_b
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat", "chatId": TARGET, "spaceId": "space-b"
            }),
        )
        .await
        .expect("create target chat");
    wait_for(
        || {
            a.workspace
                .doc()
                .chat(TARGET)
                .ok()
                .flatten()
                .is_some_and(|chat| chat.device_id == "dev-b")
        },
        "target ownership on sender",
    )
    .await;

    client_a
        .call(
            methods::ADD_SESSION_REF,
            serde_json::json!({ "chatId": TARGET }),
        )
        .await
        .expect("import remote session");
    wait_for(
        || {
            b.workspace
                .doc()
                .read_session_refs()
                .unwrap_or_default()
                .iter()
                .any(|session_ref| session_ref.chat_id == TARGET)
        },
        "imported membership on target host",
    )
    .await;

    let session_link = bridge_session(&a, &b, TARGET);
    a.doc_host
        .queue_command(
            TARGET,
            SessionCommandPayload::Queue {
                prompt: "remote collaborator message".into(),
                message_id: Some("remote-message".into()),
            },
        )
        .expect("queue imported session message");
    let target_handle = b.doc_host.open(TARGET).unwrap();
    let target_doc = target_handle.doc();
    wait_for(
        || {
            target_doc
                .read_commands()
                .unwrap_or_default()
                .iter()
                .any(|command| command.status == SessionCommandStatus::Applied)
        },
        "remote chat command execution",
    )
    .await;
    assert!(
        target_doc
            .read_entries()
            .unwrap()
            .iter()
            .any(|entry| entry.id == "remote-message"),
        "the remote host must persist the collaborator's user turn"
    );

    workspace_link.abort();
    session_link.abort();
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn claim_on_first_command_creates_the_chat_row() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = assemble(dir_a.path(), "dev-a");
    let b = assemble(dir_b.path(), "dev-b");
    let link = bridge(&a, &b);

    // No CreateChat: the first run command claims the chat under A's device id.
    queue_run(&a, "chat-claimed", "m-1");
    wait_for(
        || {
            b.workspace
                .doc()
                .chat("chat-claimed")
                .ok()
                .flatten()
                .is_some_and(|c| c.device_id == "dev-a" && c.cwd.as_deref() == Some("/tmp"))
        },
        "claimed chat row on B",
    )
    .await;

    link.abort();
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn imported_session_ref_watches_and_never_claims_host_placement() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), "dev-a");
    const SESSION_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    const COPIED_SESSION_ID: &str = "AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE";
    let client = comet_rpc::memory_client(core.rpc_service());
    let mut refs = client
        .subscribe(methods::WATCH_SESSION_REFS, serde_json::Value::Null)
        .await
        .expect("subscribe session refs");
    assert_eq!(refs.recv().await.unwrap(), serde_json::json!([]));

    let added = client
        .call(
            methods::ADD_SESSION_REF,
            serde_json::json!({ "chatId": COPIED_SESSION_ID }),
        )
        .await
        .expect("add session ref");
    assert_eq!(added["chatId"], SESSION_ID);
    let watched = refs.recv().await.expect("updated session refs");
    assert_eq!(watched.as_array().map(Vec::len), Some(1));
    assert_eq!(watched[0]["chatId"], SESSION_ID);

    assert!(
        core.workspace.doc().chat(SESSION_ID).unwrap().is_none(),
        "membership must not create a Chat row"
    );
    assert!(!core.workspace.is_host(SESSION_ID));
    core.workspace
        .claim_chat(SESSION_ID, Some("/tmp"))
        .expect("imported claim is a no-op");
    assert!(
        core.workspace.doc().chat(SESSION_ID).unwrap().is_none(),
        "imported session must remain non-hosted"
    );

    queue_run(&core, SESSION_ID, "m-imported");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let handle = core.doc_host.open(SESSION_ID).unwrap();
    assert_eq!(
        handle.doc().read_commands().unwrap()[0].status,
        SessionCommandStatus::Pending
    );
    assert!(handle.doc().read_entries().unwrap().is_empty());

    let removed = client
        .call(
            methods::REMOVE_SESSION_REF,
            serde_json::json!({ "chatId": COPIED_SESSION_ID }),
        )
        .await
        .expect("remove session ref");
    assert_eq!(removed, serde_json::json!({ "removed": true }));
    assert_eq!(refs.recv().await.unwrap(), serde_json::json!([]));
    assert!(
        core.workspace.is_host(SESSION_ID),
        "absence from both maps preserves the local-create fallback"
    );

    core.shutdown().await;
}

#[tokio::test]
async fn non_host_engine_leaves_remote_chats_commands_alone() {
    let dir_a = tempfile::tempdir().unwrap();
    let a = assemble(dir_a.path(), "dev-a");

    // The workspace says dev-b hosts this chat (via its dev-b space); a run
    // command in A's local copy of the session doc must NOT execute on A
    // (is_host gating).
    a.workspace
        .create_space("space-remote", "dev-b", "/tmp/remote", None, false)
        .expect("create remote space row");
    a.workspace
        .create_chat("chat-remote", "space-remote", None, None)
        .expect("create remote-hosted chat row");
    queue_run(&a, "chat-remote", "m-1");

    tokio::time::sleep(Duration::from_millis(400)).await;
    let handle = a.doc_host.open("chat-remote").expect("open chat");
    let commands = handle.doc().read_commands().expect("read commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].status,
        SessionCommandStatus::Pending,
        "command must stay pending"
    );
    let entries = handle.doc().read_entries().expect("read entries");
    assert!(
        entries.is_empty(),
        "non-host must not write entries: {entries:#?}"
    );
    assert!(a.sessions.session_status("chat-remote").is_none());

    a.shutdown().await;
}

#[tokio::test]
async fn chat_config_selects_the_run_harness() {
    let dir_a = tempfile::tempdir().unwrap();
    let a = assemble(dir_a.path(), "dev-a"); // default harness = Claude Code ("Hello")

    a.workspace
        .create_space("space-cfg", "dev-a", "/tmp/cfg", None, false)
        .expect("create space");
    a.workspace
        .create_chat(
            "chat-cfg",
            "space-cfg",
            Some(ChatConfig {
                harness: HarnessId::Codex,
                model: None,
                reasoning: None,
                agent_account_id: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
            }),
            None,
        )
        .expect("create configured chat");
    queue_run(&a, "chat-cfg", "m-1");

    // The configured harness (Codex, "From codex") ran — not the default Claude Code.
    let handle = a.doc_host.open("chat-cfg").expect("open chat");
    wait_for(
        || {
            handle.doc().read_entries().unwrap_or_default().iter().any(|e| {
                e.parts.iter().any(
                    |p| matches!(p, comet_doc::MessagePart::Text { text, .. } if text == "From codex"),
                )
            })
        },
        "configured-harness output",
    )
    .await;

    a.shutdown().await;
}

/// Live-edge variant: the same convergence through a real workspace room. Requires
/// the TS edge (`wrangler dev` in `edge/` with AUTH_MODE=dev):
///
/// ```sh
/// COMET_EDGE_WS=ws://127.0.0.1:8787 cargo test -p comet-engine -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires a live edge: set COMET_EDGE_WS (e.g. ws://127.0.0.1:8787)"]
async fn two_engines_converge_through_a_real_workspace_room() {
    use comet_engine::doc_host::EdgeConfig;

    let base = std::env::var("COMET_EDGE_WS")
        .expect("set COMET_EDGE_WS to the edge origin, e.g. ws://127.0.0.1:8787");
    let org = format!("org-{}", uuid::Uuid::new_v4().simple());

    let assemble_live = |dir: &std::path::Path, device_id: &str, user: &str| {
        std::fs::create_dir_all(dir).expect("create data dir");
        std::fs::write(dir.join("device-id"), device_id).expect("write device id");
        // Dev-mode bearer `user@org` carries the org claim the workspace route checks.
        let edge = Some(EdgeConfig::with_static_token(
            base.clone(),
            format!("{user}@{org}"),
        ));
        EngineCore::assemble_with_identity(
            dir,
            registry(),
            HarnessId::ClaudeCode,
            edge,
            &org,
            user,
            comet_proto::RuntimeProfile::Mock,
        )
        .expect("engine core assembles")
    };

    // Workspace docs are per-user (`ws3/{org}/{user}`): convergence is across
    // ONE user's devices — two engines, same user, different device ids.
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = assemble_live(dir_a.path(), "dev-live-a", "alice");
    let b = assemble_live(dir_b.path(), "dev-live-b", "alice");

    // Both device rows converge through the real room.
    for core in [&a, &b] {
        wait_for(
            || {
                let ids: Vec<String> = core
                    .workspace
                    .doc()
                    .read_devices()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|d| d.id)
                    .collect();
                ids == ["dev-live-a", "dev-live-b"]
            },
            "both device rows through the edge",
        )
        .await;
    }

    // A rename from B lands on A.
    b.workspace
        .rename_device("dev-live-a", "renamed by b")
        .expect("rename");
    wait_for(
        || {
            a.workspace
                .doc()
                .read_devices()
                .unwrap_or_default()
                .iter()
                .any(|d| d.id == "dev-live-a" && d.name == "renamed by b")
        },
        "device rename through the edge",
    )
    .await;

    a.shutdown().await;
    b.shutdown().await;
}
