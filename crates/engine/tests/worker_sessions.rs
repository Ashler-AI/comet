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

fn git(repo: &Path, args: &[&str]) -> String {
    let result = std::process::Command::new("git").args(args).current_dir(repo)
        .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false").output().unwrap();
    assert!(result.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&result.stderr));
    String::from_utf8(result.stdout).unwrap()
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
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = client.call(methods::READ_WORKER_SESSION, identity()).await.unwrap();
            if snapshot["latestEvent"]["event"]["text"] == "completed successfully" {
                assert_eq!(snapshot["state"], "busy", "agent prose is not a terminal outcome");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("live misleading completion text");
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
async fn rejected_active_worker_recovery_preserves_pending_commands() {
    use comet_doc::{SessionCommandEntry, SessionCommandPayload, SessionCommandStatus, SessionControlAction};

    let root = tempfile::tempdir().unwrap();
    let project = init_repo(root.path());
    let (core, requests) = engine(root.path());
    owners(&core, &project);
    let client = comet_rpc::memory_client(core.rpc_service());
    client.call(methods::ENSURE_WORKER_SESSION, spec(&project)).await.unwrap();
    send(&client, "active-during-recover", "[hold]").await.unwrap();
    state(&client, "busy").await;

    // A remote-owned command remains pending on this host, so the local
    // executor cannot settle this queued work while recovery is being tested.
    let pending = SessionCommandEntry {
        id: "retained-pending-command".into(),
        payload: SessionCommandPayload::Control {
            session_id: WORKER.into(), owner_device_id: "remote-owner".into(),
            actor_device_id: core.device_id.clone(), actor_subject: "test-actor".into(),
            grant_id: "remote-queue-grant".into(), source: comet_proto::AgentSessionSource::Scaffold,
            action: Box::new(SessionControlAction::Queue {
                prompt: "retain queued work".into(), message_id: Some("retained-message".into()),
            }),
        },
        issued_by: core.device_id.clone(), issued_at: chrono::Utc::now().timestamp_millis(),
        based_on: None, expires_at: None, status: SessionCommandStatus::Pending, resolution: None,
    };
    core.doc_host.open(WORKER).unwrap().doc().queue_command(&pending).unwrap();
    let mut paused = core.workspace.doc().worker_binding(WORKER).unwrap().unwrap();
    paused.paused = true;
    core.workspace.doc().set_worker_binding(&paused).unwrap();
    let error = client.call(methods::CONTROL_WORKER_SESSION,
        json!({"chatId":WORKER,"ownerChatId":OWNER,"action":"recover"})).await.unwrap_err();
    assert!(error.to_string().contains("worker_still_active"));
    assert_eq!(core.doc_host.command_entry(WORKER, &pending.id).await.unwrap(), Some(pending));
    assert_eq!(core.workspace.doc().worker_binding(WORKER).unwrap(), Some(paused));
    let still_active = client.call(methods::READ_WORKER_SESSION, identity()).await.unwrap();
    assert_eq!(still_active["state"], "busy", "rejected recovery must not stop the active run");
    assert_eq!(requests.lock().len(), 1, "rejected recovery must not start another run");
    control(&client, "interrupt").await;
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

fn worker_request(path: &Path, prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(), model: Some(MODEL.into()), agent_account_id: None,
        reasoning: Some(ReasoningLevel::XHigh), model_options: Default::default(),
        cwd: path.to_string_lossy().into_owned(), sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
        auto_approve: true, attachments: Vec::new(), resume: None,
    }
}

#[tokio::test]
async fn worker_send_and_dispatch_reject_replaced_git_identity_without_discarding_work() {
    for change in ["sibling-checkout", "foreign-checkout", "moved-backlink", "moved-git-dir", "foreign-common-dir",
        "worktree-root", #[cfg(unix)] "symlink-git-dir"] {
        let root = tempfile::tempdir().unwrap();
        let project = init_repo(root.path());
        let (core, requests) = engine(root.path()); owners(&core, &project);
        let client = comet_rpc::memory_client(core.rpc_service());
        let created = client.call(methods::ENSURE_WORKER_SESSION, spec(&project)).await.unwrap();
        let path = Path::new(created["chat"]["cwd"].as_str().unwrap());
        let git_file = path.join(".git");
        let original_git = std::fs::read(&git_file).unwrap();
        let git_dir = core.repos.checkout_identity(path).await.unwrap().git_dir;
        let sibling = project.parent().unwrap().join("sibling");
        git(&project, &["worktree", "add", "--detach", sibling.to_str().unwrap(), "main"]);
        let foreign_root = root.path().join("foreign");
        let foreign = init_repo(&foreign_root);
        let foreign_checkout = foreign.parent().unwrap().join("linked");
        git(&foreign, &["worktree", "add", "--detach", foreign_checkout.to_str().unwrap(), "main"]);

        // Identity is independent of branch name, commits, dirty tracked files
        // and untracked files. Exercise the real dispatch path before tampering.
        git(path, &["branch", "-m", "renamed-worker"]);
        std::fs::write(path.join("tracked"), "committed worker work").unwrap();
        git(path, &["add", "tracked"]);
        git(path, &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-m", "worker work"]);
        std::fs::write(path.join("tracked"), "dirty worker work").unwrap();
        std::fs::write(path.join("unlanded"), "untracked worker work").unwrap();
        core.sessions.dispatch(WORKER, HarnessId::Omp, worker_request(path, "original checkout"),
            Some("original-turn".into())).await.unwrap();
        state(&client, "completed").await;
        assert_eq!(requests.lock().len(), 1);

        let backlink = git_dir.join("gitdir");
        let original_backlink = std::fs::read(&backlink).unwrap();
        let common_file = git_dir.join("commondir");
        let original_common = std::fs::read(&common_file).unwrap();
        let moved_git_dir = git_dir.with_file_name("moved-worker-metadata");
        match change {
            "sibling-checkout" => std::fs::write(&git_file, std::fs::read(sibling.join(".git")).unwrap()).unwrap(),
            "foreign-checkout" => std::fs::write(&git_file, std::fs::read(foreign_checkout.join(".git")).unwrap()).unwrap(),
            "moved-backlink" => std::fs::write(&backlink, format!("{}\n", sibling.join(".git").display())).unwrap(),
            "moved-git-dir" => {
                std::fs::rename(&git_dir, &moved_git_dir).unwrap();
                std::fs::write(&git_file, format!("gitdir: {}\n", moved_git_dir.display())).unwrap();
            }
            #[cfg(unix)]
            "symlink-git-dir" => {
                std::fs::rename(&git_dir, &moved_git_dir).unwrap();
                std::os::unix::fs::symlink(&moved_git_dir, &git_dir).unwrap();
            }
            "foreign-common-dir" => std::fs::write(&common_file, format!("{}\n", foreign.join(".git").display())).unwrap(),
            "worktree-root" => {
                git(&project, &["config", "extensions.worktreeConfig", "true"]);
                git(path, &["config", "--worktree", "core.worktree", project.to_str().unwrap()]);
                assert_eq!(Path::new(git(path, &["rev-parse", "--show-toplevel"]).trim()), project);
            }
            _ => unreachable!(),
        }
        assert!(send(&client, "rejected-peer", "must not queue").await.is_err(), "{change}");
        assert!(core.sessions.dispatch(WORKER, HarnessId::Omp, worker_request(path, "must not run"),
            Some("rejected-direct".into())).await.is_err(), "{change}");
        assert_eq!(requests.lock().len(), 1, "{change} must not invoke the harness");
        let handle = core.doc_host.open(WORKER).unwrap();
        assert!(handle.doc().read_commands().unwrap().is_empty(), "{change} must not queue a command");
        assert!(!handle.doc().read_entry_window(None, 64).unwrap().entries.iter()
            .any(|entry| entry.id == "rejected-direct" || entry.id == "rejected-peer"), "{change} must not queue a turn");
        assert!(client.call(methods::READ_WORKER_SESSION, identity()).await.is_err(), "{change}");
        control(&client, "interrupt").await;
        assert!(client.call(methods::CONTROL_WORKER_SESSION,
            json!({"chatId":WORKER,"ownerChatId":OWNER,"action":"recover"})).await.is_err(), "{change}");

        // Restore only the tampered metadata, not the working tree. Recovery
        // must admit the original renamed checkout and preserve all user work.
        if change == "moved-git-dir" { std::fs::rename(&moved_git_dir, &git_dir).unwrap(); }
        if change == "symlink-git-dir" {
            std::fs::remove_file(&git_dir).unwrap();
            std::fs::rename(&moved_git_dir, &git_dir).unwrap();
        }
        std::fs::write(&git_file, &original_git).unwrap();
        std::fs::write(&backlink, &original_backlink).unwrap();
        std::fs::write(&common_file, &original_common).unwrap();
        if change == "worktree-root" {
            std::fs::remove_file(git_dir.join("config.worktree")).unwrap();
        }
        control(&client, "recover").await;
        send(&client, "retained-peer", "continue retained work").await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while requests.lock().len() != 2 { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
        let completed = state(&client, "completed").await;
        assert!(completed["commands"].as_array().unwrap().iter().any(|command| command["id"] == "retained-peer"));
        assert_eq!(std::fs::read_to_string(path.join("tracked")).unwrap(), "dirty worker work");
        assert_eq!(std::fs::read_to_string(path.join("unlanded")).unwrap(), "untracked worker work");
        assert_eq!(git(path, &["show", "HEAD:tracked"]), "committed worker work");
        assert_eq!(git(path, &["branch", "--show-current"]).trim(), "renamed-worker");
        core.shutdown().await;
    }
}

#[tokio::test]
async fn worker_dispatch_rejects_nonlocal_owner_binding() {
    let root = tempfile::tempdir().unwrap();
    let project = init_repo(root.path());
    let (core, requests) = engine(root.path()); owners(&core, &project);
    let client = comet_rpc::memory_client(core.rpc_service());
    let created = client.call(methods::ENSURE_WORKER_SESSION, spec(&project)).await.unwrap();
    let path = Path::new(created["chat"]["cwd"].as_str().unwrap());
    let original = core.workspace.doc().worker_binding(WORKER).unwrap().unwrap();
    let mut foreign = original.clone();
    foreign.owner_device_id = "foreign-device".into();
    core.workspace.doc().set_worker_binding(&foreign).unwrap();
    assert!(send(&client, "foreign-owner", "must not queue").await.is_err());
    assert!(core.sessions.dispatch(WORKER, HarnessId::Omp, worker_request(path, "must not run"), None).await.is_err());
    assert!(requests.lock().is_empty());
    assert!(core.doc_host.open(WORKER).unwrap().doc().read_commands().unwrap().is_empty());
    core.workspace.doc().set_worker_binding(&original).unwrap();
    core.workspace.set_chat_host(OWNER, "foreign-device").unwrap();
    assert!(send(&client, "moved-owner", "must not queue").await.is_err());
    assert!(core.sessions.dispatch(WORKER, HarnessId::Omp, worker_request(path, "must not run"), None).await.is_err());
    assert!(requests.lock().is_empty());
    core.workspace.set_chat_host(OWNER, &core.device_id).unwrap();
    core.sessions.dispatch(WORKER, HarnessId::Omp, worker_request(path, "restored local owner"), None).await.unwrap();
    state(&client, "completed").await;
    assert_eq!(requests.lock().len(), 1);
    core.shutdown().await;
}

#[tokio::test]
async fn stopped_provisioning_recovers_same_reservation_without_resetting_partial_checkout() {
    for action in ["interrupt", "close"] {
        let root = tempfile::tempdir().unwrap();
        let project = init_repo(root.path());
        let (core, requests) = engine(root.path()); owners(&core, &project);
        let client = comet_rpc::memory_client(core.rpc_service());
        let mut request = spec(&project);
        request["baseRef"] = json!("not-yet-created");
        assert!(client.call(methods::ENSURE_WORKER_SESSION, request.clone()).await.is_err());
        let reserved = core.workspace.doc().worker_binding(WORKER).unwrap().unwrap();
        assert!(reserved.worktree.is_none());
        let retained = project.parent().unwrap().join("worktrees/project").join(format!("worker-{WORKER}"));
        std::fs::create_dir_all(retained.parent().unwrap()).unwrap();
        git(&project, &["branch", "not-yet-created", "main"]);
        git(&project, &["worktree", "add", "-b", &format!("comet/worker-{WORKER}"),
            retained.to_str().unwrap(), "not-yet-created"]);
        std::fs::write(retained.join("partial-unlanded"), "retain partial provisioning").unwrap();
        let retained_head = git(&retained, &["rev-parse", "HEAD"]);
        control(&client, action).await;
        assert!(client.call(methods::ENSURE_WORKER_SESSION, request.clone()).await.is_err());
        control(&client, "recover").await;
        assert!(send(&client, "not-bound", "must not execute").await.is_err());
        let recovered = client.call(methods::ENSURE_WORKER_SESSION, request).await.unwrap();
        assert_eq!(recovered["binding"]["chatId"], WORKER);
        assert_eq!(recovered["binding"]["ownerChatId"], reserved.owner_chat_id);
        assert_eq!(recovered["chat"]["cwd"].as_str().unwrap(), retained.to_str().unwrap());
        assert_eq!(git(&retained, &["rev-parse", "HEAD"]), retained_head);
        assert_eq!(std::fs::read_to_string(retained.join("partial-unlanded")).unwrap(), "retain partial provisioning");
        assert!(requests.lock().is_empty(), "recovery and ensure never start inference");
        send(&client, "explicit-after-recover", "continue").await.unwrap();
        state(&client, "completed").await;
        assert_eq!(requests.lock().len(), 1);
        core.shutdown().await;
    }
}

#[cfg(unix)]
mod async_admission {
    use super::*;
    use comet_doc::SessionCommandPayload;
    use std::ffi::CString;
    use std::io::Write;
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::{atomic::{AtomicBool, Ordering}, mpsc};
    use std::time::Instant;

    const WAIT: Duration = Duration::from_secs(10);

    // Only this disposable worker's Git config includes the FIFO. No PATH or
    // process-global environment changes, shell wrappers, or fake Git results.
    // The writer opens nonblocking: success proves Git reached the config read.
    // An independent watchdog closes it even if a regressed synchronous Git
    // call blocks the current-thread runtime. Drop also joins that thread, so
    // setup errors and cancelled tests cannot strand a writer/blocking task.
    struct GitStall {
        config: PathBuf,
        original_config: Option<Vec<u8>>,
        fifo: PathBuf,
        writer: Arc<Mutex<Option<std::fs::File>>>,
        reached: Option<tokio::sync::oneshot::Receiver<Result<(), String>>>,
        stop: mpsc::Sender<()>,
        watchdog: Option<std::thread::JoinHandle<()>>,
        expired: Arc<AtomicBool>,
    }

    impl GitStall {
        async fn install(core: &EngineCore, project: &Path, worker: &Path) -> Self {
            let git_dir = core.repos.checkout_identity(worker).await.unwrap().git_dir;
            git(project, &["config", "extensions.worktreeConfig", "true"]);
            let config = git_dir.join("config.worktree");
            let original_config = match std::fs::read(&config) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("read worker config: {error}"),
            };
            let fifo = git_dir.join("admission-config.fifo");
            let c_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0,
                "mkfifo: {}", std::io::Error::last_os_error());
            let writer = Arc::new(Mutex::new(None));
            let expired = Arc::new(AtomicBool::new(false));
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let (stop_tx, stop_rx) = mpsc::channel();
            let mut stall = Self {
                config, original_config, fifo, writer, reached: Some(ready_rx),
                stop: stop_tx, watchdog: None, expired,
            };
            let mut contents = stall.original_config.clone().unwrap_or_default();
            contents.extend_from_slice(format!("\n[include]\n\tpath = {}\n",
                serde_json::to_string(stall.fifo.to_str().unwrap()).unwrap()).as_bytes());
            std::fs::write(&stall.config, contents).unwrap();
            let fifo = stall.fifo.clone();
            let writer = stall.writer.clone();
            let expired = stall.expired.clone();
            let config = stall.config.clone();
            let original_config = stall.original_config.clone();
            stall.watchdog = Some(std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(8);
                loop {
                    if stop_rx.try_recv().is_ok() { return; }
                    if Instant::now() >= deadline {
                        expired.store(true, Ordering::SeqCst);
                        let _ = ready_tx.send(Err("Git never opened the FIFO".into()));
                        return;
                    }
                    match std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(&fifo) {
                        Ok(file) => {
                            *writer.lock() = Some(file);
                            let _ = ready_tx.send(Ok(()));
                            if matches!(stop_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                                Err(mpsc::RecvTimeoutError::Timeout)) {
                                expired.store(true, Ordering::SeqCst);
                            }
                            // A blocking implementation may run another Git
                            // probe before the Tokio test can regain control.
                            restore_config(&config, original_config.as_deref());
                            writer.lock().take();
                            return;
                        }
                        Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => {
                            let _ = ready_tx.send(Err(error.to_string()));
                            return;
                        }
                    }
                }
            }));
            stall
        }

        async fn reached<F: Future>(&mut self, admission: Pin<&mut F>) {
            let ready = self.reached.take().unwrap();
            tokio::select! {
                result = ready => result.expect("FIFO watchdog disappeared").expect("FIFO reader handshake"),
                _ = admission => panic!("admission finished before the stalled Git read"),
            }
            assert!(!self.expired.load(Ordering::SeqCst), "runtime was blocked until watchdog released Git");
        }

        fn restore_config(&self) {
            restore_config(&self.config, self.original_config.as_deref());
        }

        fn release(&self) {
            // Future Git probes must not open a FIFO with no remaining writer.
            self.restore_config();
            self.writer.lock().take();
        }

        async fn assert_reader_killed(&self) {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let result = self.writer.lock().as_mut().expect("watchdog released writer")
                        .write_all(b"#\n");
                    match result {
                        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => break,
                        Ok(()) => {},
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {},
                        Err(error) => panic!("FIFO write: {error}"),
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }).await.expect("cancelled Git must close its FIFO reader, not survive in the background");
            assert!(!self.expired.load(Ordering::SeqCst));
        }
    }

    impl Drop for GitStall {
        fn drop(&mut self) {
            self.restore_config();
            let _ = self.stop.send(());
            if let Some(watchdog) = self.watchdog.take() { let _ = watchdog.join(); }
            self.writer.lock().take();
            let _ = std::fs::remove_file(&self.fifo);
        }
    }

    fn restore_config(path: &Path, original: Option<&[u8]>) {
        match original {
            Some(bytes) => { let _ = std::fs::write(path, bytes); }
            None => { let _ = std::fs::remove_file(path); }
        }
    }

    // A std mutex held across an await can deadlock the runtime itself; no
    // Tokio timeout can rescue that regression. Run lock-contention cases in
    // an exact-filtered child test process with a parent-side wall-clock bound.
    // TMPDIR is scoped only to that child so its disposable repos are removed
    // by the parent even if the deadlocked child must be killed.
    fn supervise_contention(name: &str) -> bool {
        const MARKER: &str = "COMET_WORKER_ADMISSION_CHILD";
        if std::env::var(MARKER).ok().as_deref() == Some(name) { return false; }
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let mut child = Child(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("async_admission::{name}"), "--nocapture"])
            .env(MARKER, name).env("TMPDIR", root.path())
            .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false").spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success(), "{name}: child regression failed: {status}");
                return true;
            }
            assert!(Instant::now() < deadline, "{name}: command mutex deadlocked the current-thread runtime");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn payload(id: &str, text: &str) -> SessionCommandPayload {
        SessionCommandPayload::PeerMessage {
            source_chat_id: OWNER.into(), thread_id: id.into(), reply_to: None,
            hop_count: 0, text: text.into(),
        }
    }

    async fn create_worker(core: &EngineCore, project: &Path) -> (RpcClient, PathBuf) {
        owners(core, project);
        let client = comet_rpc::memory_client(core.rpc_service());
        let created = client.call(methods::ENSURE_WORKER_SESSION, spec(project)).await.unwrap();
        let worker = PathBuf::from(created["chat"]["cwd"].as_str().unwrap());
        (client, worker)
    }

    fn assert_not_admitted(core: &EngineCore, requests: &Mutex<Vec<RunRequest>>) {
        let handle = core.doc_host.open(WORKER).unwrap();
        assert!(handle.doc().read_commands().unwrap().is_empty(), "pre-admission rejection must not append a command");
        assert!(handle.doc().read_entry_window(None, 64).unwrap().entries.is_empty(),
            "pre-admission rejection must not append a turn");
        assert!(requests.lock().is_empty(), "pre-admission rejection must not invoke the harness");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_git_yields_and_dropped_admission_kills_child_without_append() {
        let root = tempfile::tempdir().unwrap();
        let project = init_repo(root.path());
        let (core, requests) = engine(root.path());
        let (_client, worker) = create_worker(&core, &project).await;
        let mut stall = GitStall::install(&core, &project, &worker).await;
        {
            // Drop the actual DocHost future, not an RPC transport waiter.
            let admission = core.doc_host.queue_command_with_id(WORKER, "dropped", payload("dropped", "must not run"));
            tokio::pin!(admission);
            stall.reached(admission.as_mut()).await;
            let heartbeat = tokio::spawn(async { tokio::task::yield_now().await; 42 });
            assert_eq!(tokio::time::timeout(Duration::from_secs(1), heartbeat).await.unwrap().unwrap(), 42);
            assert!(futures::poll!(admission.as_mut()).is_pending(), "Git must remain stalled while scheduler progresses");
            assert_not_admitted(&core, &requests);
        }
        stall.assert_reader_killed().await;
        assert!(tokio::time::timeout(Duration::from_secs(1), core.doc_host.command_entry(WORKER, "dropped"))
            .await.unwrap().unwrap().is_none(), "cancelled admission must release the command lock");
        assert_not_admitted(&core, &requests);
        drop(stall);
        core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_git_deadline_kills_child_and_fails_admission_closed() {
        let root = tempfile::tempdir().unwrap();
        let project = init_repo(root.path());
        let (core, requests) = engine(root.path());
        let (_client, worker) = create_worker(&core, &project).await;
        let mut stall = GitStall::install(&core, &project, &worker).await;
        let admission = core.doc_host.queue_command_with_id(WORKER, "deadline", payload("deadline", "must not run"));
        tokio::pin!(admission);
        stall.reached(admission.as_mut()).await;
        // The outer bound is deliberately longer than Repos' own path-probe
        // deadline. An Err returned by admission proves its timeout, not ours.
        assert!(tokio::time::timeout(Duration::from_secs(5), admission.as_mut()).await
            .expect("Git path-probe deadline must be enforced").is_err());
        stall.assert_reader_killed().await;
        assert_not_admitted(&core, &requests);
        drop(stall);
        core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn git_config_failure_rejects_admission_without_append_or_harness() {
        let root = tempfile::tempdir().unwrap();
        let project = init_repo(root.path());
        let (core, requests) = engine(root.path());
        let (_client, worker) = create_worker(&core, &project).await;
        let mut stall = GitStall::install(&core, &project, &worker).await;
        let admission = core.doc_host.queue_command_with_id(WORKER, "git-failed", payload("git-failed", "must not run"));
        tokio::pin!(admission);
        stall.reached(admission.as_mut()).await;
        stall.writer.lock().as_mut().unwrap().write_all(b"[unterminated section\n").unwrap();
        stall.release();
        assert!(tokio::time::timeout(WAIT, admission.as_mut()).await.unwrap().is_err());
        assert_not_admitted(&core, &requests);
        drop(stall);
        core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_command_retries_share_one_admission_and_reject_conflicting_payload() {
        if supervise_contention("concurrent_command_retries_share_one_admission_and_reject_conflicting_payload") { return; }
        let root = tempfile::tempdir().unwrap();
        let project = init_repo(root.path());
        let (core, _requests) = engine(root.path());
        let (_client, worker) = create_worker(&core, &project).await;
        let mut stall = GitStall::install(&core, &project, &worker).await;
        let first = core.doc_host.queue_command_with_id(WORKER, "same-id", payload("same-id", "[hold]"));
        tokio::pin!(first);
        stall.reached(first.as_mut()).await;
        let retry = core.doc_host.queue_command_with_id(WORKER, "same-id", payload("same-id", "[hold]"));
        let conflict = core.doc_host.queue_command_with_id(WORKER, "same-id", payload("same-id", "different"));
        let lookup = core.doc_host.command_entry(WORKER, "same-id");
        tokio::pin!(retry, conflict, lookup);
        assert!(futures::poll!(retry.as_mut()).is_pending());
        assert!(futures::poll!(conflict.as_mut()).is_pending());
        assert!(futures::poll!(lookup.as_mut()).is_pending(), "command_entry must share the admission lock");
        stall.release();
        let (first, retry, conflict, lookup) = tokio::time::timeout(WAIT, async {
            tokio::join!(first.as_mut(), retry.as_mut(), conflict.as_mut(), lookup.as_mut())
        }).await.unwrap();
        let first = first.unwrap();
        let retry = retry.unwrap();
        assert_eq!(first.id, retry.id);
        assert_eq!(first.payload, retry.payload);
        assert_eq!(first.issued_at, retry.issued_at);
        assert!(conflict.unwrap_err().to_string().contains("command_id_conflict"));
        assert_eq!(lookup.unwrap().unwrap().payload, first.payload);
        let commands = core.doc_host.open(WORKER).unwrap().doc().read_commands().unwrap();
        assert_eq!(commands.len(), 1, "concurrent retries must produce one durable admission");
        assert_eq!(commands[0].payload, first.payload);
        drop(stall);
        core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn awaited_git_rechecks_worker_binding_pause_and_config_before_admission() {
        for change in ["binding", "paused", "closed", "config", "checkout-pointer"] {
            let root = tempfile::tempdir().unwrap();
            let project = init_repo(root.path());
            let (core, requests) = engine(root.path());
            let (_client, worker) = create_worker(&core, &project).await;
            let mut stall = GitStall::install(&core, &project, &worker).await;
            let admission = core.doc_host.queue_command_with_id(WORKER, "changed", payload("changed", "must not run"));
            tokio::pin!(admission);
            stall.reached(admission.as_mut()).await;
            let mut binding = core.workspace.doc().worker_binding(WORKER).unwrap().unwrap();
            match change {
                "binding" => {
                    binding.owner_chat_id = OTHER.into();
                    core.workspace.doc().set_worker_binding(&binding).unwrap();
                }
                "paused" => {
                    binding.paused = true;
                    core.workspace.doc().set_worker_binding(&binding).unwrap();
                }
                "config" => {
                    let mut config = binding.config;
                    config.reasoning = Some(ReasoningLevel::Low);
                    core.workspace.set_chat_config(WORKER, &config).unwrap();
                }
                "closed" => {
                    binding.closed = true;
                    core.workspace.doc().set_worker_binding(&binding).unwrap();
                }
                "checkout-pointer" => {
                    std::fs::write(worker.join(".git"), "gitdir: /missing-worker-checkout\n").unwrap();
                }
                _ => unreachable!(),
            }
            stall.release();
            let error = tokio::time::timeout(WAIT, admission.as_mut()).await.unwrap().unwrap_err();
            if change != "checkout-pointer" {
                assert!(error.to_string().contains("worker_binding_changed"), "{change}: {error}");
            }
            assert_not_admitted(&core, &requests);
            drop(stall);
            core.shutdown().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_interrupt_and_close_fence_admission_while_waiting_for_command_lock() {
        if supervise_contention("worker_interrupt_and_close_fence_admission_while_waiting_for_command_lock") { return; }
        for action in ["interrupt", "close"] {
            let root = tempfile::tempdir().unwrap();
            let project = init_repo(root.path());
            let (core, requests) = engine(root.path());
            let (client, worker) = create_worker(&core, &project).await;
            let mut stall = GitStall::install(&core, &project, &worker).await;
            let admission = core.doc_host.queue_command_with_id(WORKER, "stopped", payload("stopped", "must not run"));
            tokio::pin!(admission);
            stall.reached(admission.as_mut()).await;
            let stopping = control(&client, action);
            tokio::pin!(stopping);
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    tokio::select! {
                        _ = stopping.as_mut() => panic!("lifecycle bypassed the held command lock"),
                        _ = tokio::task::yield_now() => {},
                    }
                    if core.workspace.doc().worker_binding(WORKER).unwrap().unwrap().paused { break; }
                }
            }).await.expect("lifecycle must publish its pause fence without blocking the runtime");
            let lookup = core.doc_host.command_entry(WORKER, "stopped");
            tokio::pin!(lookup);
            assert!(futures::poll!(lookup.as_mut()).is_pending());
            stall.release();
            let (admission, stopped, lookup) = tokio::time::timeout(WAIT, async {
                tokio::join!(admission.as_mut(), stopping.as_mut(), lookup.as_mut())
            }).await.unwrap();
            assert!(admission.is_err());
            assert_eq!(stopped["state"], if action == "close" { "closed" } else { "interrupted" });
            assert!(lookup.unwrap().is_none());
            assert_not_admitted(&core, &requests);
            drop(stall);
            core.shutdown().await;
        }
    }
}
