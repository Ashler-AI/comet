//! Native worker contract regressions. No provider, remote service or desktop required.
use std::path::Path;
use std::sync::Arc;
use parking_lot::Mutex;
use std::time::Duration;
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use comet_engine::{EngineCore, HarnessRegistry, Repos};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode, UserInputQuestion};
use comet_rpc::{RpcClient, methods};
use serde_json::{Value, json};

const OWNER: &str = "00000000-0000-4000-8000-000000000001";
const WORKER: &str = "00000000-0000-4000-8000-000000000002";
const OTHER: &str = "00000000-0000-4000-8000-000000000003";
const MODEL: &str = "openai-codex/gpt-6-astra";

struct ControlledOmp { requests: Arc<Mutex<Vec<RunRequest>>> }
#[async_trait]
impl Harness for ControlledOmp {
    fn id(&self) -> HarnessId { HarnessId::Omp }
    fn display_name(&self) -> &str { "Controlled test OMP" }
    fn supports_steering(&self) -> bool { false }
    fn steering_mode(&self) -> SteeringMode { SteeringMode::TurnBoundary }
    fn reasoning_levels(&self) -> &[ReasoningLevel] { &[ReasoningLevel::Low, ReasoningLevel::Medium, ReasoningLevel::XHigh] }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![Model { id: MODEL.into(), label: "Test catalog".into(), description: None,
            reasoning_levels: self.reasoning_levels().to_vec(), options: vec![] }])
    }
    async fn run(&self, request: RunRequest, controls: RunControls) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.requests.lock().push(request.clone());
        let started = AgentEvent::SessionStarted { harness: HarnessId::Omp, model: MODEL.into(), tools: vec![], cwd: request.cwd,
            session_id: "test-native-session".into(), assistant_message_id: uuid::Uuid::new_v4().to_string() };
        let prefix = futures::stream::iter(vec![Ok(started), Ok(AgentEvent::TextDelta { text: "completed successfully".into() })]);
        let terminal = futures::stream::once(async move {
            if request.prompt.contains("[hold]") { controls.interrupt.cancelled().await; }
            if request.prompt.contains("[wait]") {
                let _ = (controls.request_input)(vec![UserInputQuestion { id: "question".into(), header: "Choice".into(),
                    question: "Continue?".into(), options: vec!["yes".into()], multi_select: false }]).await;
            }
            Ok(AgentEvent::Done { status: if controls.interrupt.is_cancelled() { DoneStatus::Interrupted }
                else if request.prompt.contains("[fail]") { DoneStatus::Errored } else { DoneStatus::Completed },
                result: None, error: request.prompt.contains("[fail]").then(|| "provider failed".into()), session_id: None })
        });
        Ok(prefix.chain(terminal).boxed())
    }
}

fn git(repo: &Path, args: &[&str]) {
    let result = std::process::Command::new("git").args(args).current_dir(repo)
        .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false").output().unwrap();
    assert!(result.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&result.stderr));
}
fn init_repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("project");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-m", "base"]);
    std::fs::canonicalize(repo).unwrap()
}
fn engine(root: &Path) -> (EngineCore, Arc<Mutex<Vec<RunRequest>>>) {
    let requests = Arc::new(Mutex::new(vec![]));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(ControlledOmp { requests: requests.clone() }));
    let mut core = EngineCore::assemble(&root.join("data"), Arc::new(registry), HarnessId::Omp, None).unwrap();
    core.repos = Repos::with_worktrees_root(&root.join("data"), &core.device_id, root.join("worktrees"));
    (core, requests)
}
fn owners(core: &EngineCore, project: &Path) {
    core.workspace.create_space("owner-space", &core.device_id, project.to_str().unwrap(), None, true).unwrap();
    for id in [OWNER, OTHER] { core.workspace.create_chat(id, "owner-space", None, None).unwrap(); }
}
fn spec(project: &Path) -> Value {
    json!({"chatId":WORKER,"ownerChatId":OWNER,"projectPath":project,"baseRef":"main","title":"test worker","model":MODEL,"effort":"xhigh"})
}
fn identity() -> Value { json!({"chatId":WORKER,"ownerChatId":OWNER}) }
async fn state(client: &RpcClient, wanted: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = client.call(methods::READ_WORKER_SESSION, identity()).await.unwrap();
            if status["state"] == wanted { return status; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap_or_else(|_| panic!("worker did not reach {wanted}"))
}
async fn send(client: &RpcClient, id: &str, text: &str) -> Result<Value, comet_rpc::RpcError> {
    client.call(methods::SEND_PEER_MESSAGE, json!({"sourceChatId":OWNER,"targetChatId":WORKER,"commandId":id,"text":text})).await
}
async fn control(client: &RpcClient, action: &str) -> Value {
    client.call(methods::CONTROL_WORKER_SESSION, json!({"chatId":WORKER,"ownerChatId":OWNER,"action":action})).await.unwrap()
}

#[tokio::test]
async fn worker_creation_retries_preserve_checkout_config_and_ownership_across_restart() {
    let root = tempfile::tempdir().unwrap();
    let project = init_repo(root.path());
    let (core, _) = engine(root.path());
    owners(&core, &project);
    let client = comet_rpc::memory_client(core.rpc_service());
    let first = client.call(methods::ENSURE_WORKER_SESSION, spec(&project)).await.unwrap();
    let path = first["chat"]["cwd"].as_str().unwrap();
    assert_ne!(Path::new(path), project);
    std::fs::write(Path::new(path).join("unlanded"), "retain me").unwrap();
    let retry = client.call(methods::ENSURE_WORKER_SESSION, spec(&project)).await.unwrap();
    assert_eq!(first["chat"]["cwd"], retry["chat"]["cwd"]);
    let mut conflict = spec(&project); conflict["ownerChatId"] = json!(OTHER);
    assert!(client.call(methods::ENSURE_WORKER_SESSION, conflict).await.is_err());
    assert!(client.call(methods::CONTROL_WORKER_SESSION, json!({"chatId":WORKER,"ownerChatId":OTHER,"action":"close"})).await.is_err());
    let path = path.to_owned();
    // Admission is persisted by the RPC itself; do not flush/shutdown to make
    // the restart contract pass accidentally.
    drop(client); drop(core);
    tokio::task::yield_now().await;
    let (core, _) = engine(root.path());
    let client = comet_rpc::memory_client(core.rpc_service());
    let restored = client.call(methods::ENSURE_WORKER_SESSION, spec(&project)).await.unwrap();
    assert_eq!(restored["chat"]["cwd"], path);
    assert_eq!(restored["chat"]["config"]["model"], MODEL);
    assert_eq!(restored["chat"]["config"]["reasoning"], "xhigh");
    assert_eq!(std::fs::read_to_string(Path::new(&path).join("unlanded")).unwrap(), "retain me");
    core.shutdown().await;
}

#[tokio::test]
async fn worker_terminal_state_is_not_text_and_close_cannot_replay_or_discard_work() {
    let root = tempfile::tempdir().unwrap();
    let project = init_repo(root.path());
    let (core, requests) = engine(root.path()); owners(&core, &project);
    let client = comet_rpc::memory_client(core.rpc_service());
    let created = client.call(methods::ENSURE_WORKER_SESSION, spec(&project)).await.unwrap();
    let path = Path::new(created["chat"]["cwd"].as_str().unwrap());
    std::fs::write(path.join("unlanded"), "retain me").unwrap();
    send(&client, "hold-command", "[hold]").await.unwrap();
    state(&client, "busy").await; // transcript says completed, runtime has not emitted Done
    assert!(client.call(methods::CANCEL_CHAT_STARTUP, json!({"chatId":WORKER,"createdWorktree":true,"worktreePath":path})).await.is_err());
    let closed = control(&client, "close").await;
    assert_eq!(closed["state"], "closed");
    assert_eq!(std::fs::read_to_string(path.join("unlanded")).unwrap(), "retain me");
    assert!(core.workspace.read_worktree_deletions().unwrap().is_empty());
    assert!(send(&client, "after-close", "must not execute").await.is_err());
    assert_eq!(requests.lock().len(), 1);
    control(&client, "recover").await;
    assert_eq!(requests.lock().len(), 1, "recover is not implicit inference");
    send(&client, "failure-command", "[fail]").await.unwrap();
    let failed = state(&client, "failed").await;
    assert_eq!(failed["latestEvent"]["event"]["status"], "errored");
    send(&client, "complete-command", "finish").await.unwrap();
    let completed = state(&client, "completed").await;
    assert_eq!(completed["latestEvent"]["event"]["status"], "completed");
    send(&client, "complete-command", "finish").await.unwrap();
    assert!(send(&client, "complete-command", "different payload").await.is_err());
    assert_eq!(requests.lock().len(), 3);
    core.shutdown().await;
}

#[tokio::test]
async fn worker_waiting_and_durable_reply_survive_waiter_disconnect() {
    let root = tempfile::tempdir().unwrap();
    let project = init_repo(root.path());
    let (core, _) = engine(root.path()); owners(&core, &project);
    let client = comet_rpc::memory_client(core.rpc_service());
    client.call(methods::ENSURE_WORKER_SESSION, spec(&project)).await.unwrap();
    send(&client, "question-command", "[wait]").await.unwrap();
    state(&client, "waiting").await;
    client.call(methods::REPLY_PEER_MESSAGE, json!({"sessionId":WORKER,"commandId":"question-command","text":"need a decision"})).await.unwrap();
    drop(client);
    let client = comet_rpc::memory_client(core.rpc_service());
    let snapshot = client.call(methods::READ_WORKER_SESSION, identity()).await.unwrap();
    assert!(snapshot["replies"].as_array().unwrap().iter().any(|reply| reply["id"] == "reply:question-command"
        && reply["payload"]["replyTo"] == "question-command" && reply["payload"]["text"] == "need a decision"));
    control(&client, "interrupt").await;
    assert!(send(&client, "blocked-command", "must recover first").await.is_err());
    core.shutdown().await;
}
