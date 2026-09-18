//! Namespace markers exist only for executing turns, never for parked harnesses.
//! The filename carries process identity so crash recovery cannot mistake a reused
//! PID, a reboot, or another live Crew engine for our abandoned work.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use uuid::Uuid;

const PREFIX: &str = "crew-turn-v1-";

pub(super) struct NamespaceTasks {
    directory: Option<PathBuf>,
    owner: io::Result<Owner>,
    engine: Uuid,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    stopped: bool,
    turns: HashMap<String, PathBuf>,
}

struct Owner {
    boot: Uuid,
    pid: u32,
    start: u64,
}

impl NamespaceTasks {
    pub(super) fn detect() -> Self {
        let directory = Path::new("/.namespace/tasks");
        Self::new(crate::namespace_devbox().then(|| directory.to_path_buf()))
    }

    fn new(directory: Option<PathBuf>) -> Self {
        let owner = if directory.is_some() {
            current_owner()
        } else {
            Err(io::Error::other("not a Namespace Devbox"))
        };
        if let (Some(directory), Ok(owner)) = (&directory, &owner) {
            if let Err(error) = sweep_stale(directory, owner.boot, process_start) {
                tracing::warn!(%error, "could not inspect stale Crew Namespace task markers");
            }
        }
        Self {
            directory,
            owner,
            engine: Uuid::new_v4(),
            state: Mutex::new(State::default()),
        }
    }

    pub(super) fn start_turn(&self, chat: &str, run: &str) -> io::Result<()> {
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        let mut state = super::lock(&self.state);
        if state.stopped {
            return Err(io::Error::other("Crew is shutting down"));
        }
        if state.turns.contains_key(run) {
            return Ok(());
        }
        let owner = self
            .owner
            .as_ref()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let path = directory.join(format!(
            "{PREFIX}{}-{}-{}-{}-{}",
            owner.boot.simple(),
            owner.pid,
            owner.start,
            self.engine.simple(),
            Uuid::new_v4().simple(),
        ));
        // create_new never truncates somebody else's task; identity is complete
        // before creation, so even a crash during the write can be swept safely.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        state.turns.insert(run.to_owned(), path);
        if let Err(error) = writeln!(
            file,
            "{}",
            serde_json::json!({ "session": chat, "run": run })
        ) {
            // Retain failed removals for shutdown's bounded second attempt.
            remove_turn(&mut state, run);
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn finish_turn(&self, run: &str) {
        remove_turn(&mut super::lock(&self.state), run);
    }

    pub(super) fn shutdown(&self) {
        let mut state = super::lock(&self.state);
        state.stopped = true;
        state.turns.retain(|_, path| !remove_marker(path));
    }
}

impl Drop for NamespaceTasks {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn remove_turn(state: &mut State, run: &str) {
    if let Some(path) = state.turns.get(run)
        && remove_marker(path)
    {
        state.turns.remove(run);
    }
}

fn remove_marker(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "could not remove Crew Namespace task marker; Devbox may remain active");
            false
        }
    }
}

fn current_owner() -> io::Result<Owner> {
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok(Owner {
        boot: Uuid::parse_str(boot.trim()).map_err(io::Error::other)?,
        pid: std::process::id(),
        start: process_start(std::process::id())?,
    })
}

fn process_start(pid: u32) -> io::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // comm can contain spaces and closing parentheses. Field 22 is starttime;
    // the first field after the final ')' is field 3 (state).
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|start| start.parse().ok())
        .ok_or_else(|| io::Error::other("invalid process start time"))
}

fn marker_owner(name: &str) -> Option<Owner> {
    let mut fields = name.strip_prefix(PREFIX)?.split('-');
    let boot = fields.next()?;
    let pid = fields.next()?.parse().ok()?;
    let start = fields.next()?.parse().ok()?;
    let engine = fields.next()?;
    let turn = fields.next()?;
    if fields.next().is_some() || [boot, engine, turn].iter().any(|value| value.len() != 32) {
        return None;
    }
    Uuid::parse_str(engine).ok()?;
    Uuid::parse_str(turn).ok()?;
    Some(Owner {
        boot: Uuid::parse_str(boot).ok()?,
        pid,
        start,
    })
}

fn sweep_stale(
    directory: &Path,
    boot: Uuid,
    process_start: impl Fn(u32) -> io::Result<u64>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(owner) = name.to_str().and_then(marker_owner) else {
            continue;
        };
        // Never follow links or recursively delete directories in this shared
        // directory. Unknown ownership/permission errors are not proof of death.
        if !entry.file_type()?.is_file() {
            continue;
        }
        let dead = owner.boot != boot
            || match process_start(owner.pid) {
                Ok(start) => start != owner.start,
                Err(error) => error.kind() == io::ErrorKind::NotFound,
            };
        if dead {
            remove_marker(&entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tasks_in(directory: &Path) -> NamespaceTasks {
        NamespaceTasks {
            directory: Some(directory.to_path_buf()),
            owner: Ok(Owner {
                boot: Uuid::nil(),
                pid: 123,
                start: 456,
            }),
            engine: Uuid::new_v4(),
            state: Mutex::new(State::default()),
        }
    }

    #[test]
    fn turns_and_engines_remove_only_their_own_markers() {
        let directory = tempfile::tempdir().unwrap();
        let unrelated = directory.path().join("build-in-progress");
        fs::write(&unrelated, "").unwrap();
        let first = tasks_in(directory.path());
        let second = tasks_in(directory.path());
        first.start_turn("chat-a", "run-a").unwrap();
        first.start_turn("chat-a", "run-a").unwrap();
        first.start_turn("chat-b", "run-b").unwrap();
        second.start_turn("chat-a", "run-a").unwrap();
        let first_a = super::super::lock(&first.state).turns["run-a"].clone();
        let first_b = super::super::lock(&first.state).turns["run-b"].clone();
        let second_a = super::super::lock(&second.state).turns["run-a"].clone();
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 4);
        first.finish_turn("run-a");
        assert!(!first_a.exists());
        assert!(first_b.exists() && second_a.exists() && unrelated.exists());
        first.start_turn("chat-a", "run-a").unwrap();
        assert_ne!(super::super::lock(&first.state).turns["run-a"], first_a);
        first.shutdown();
        assert!(!first_b.exists());
        assert!(first.start_turn("chat-c", "run-c").is_err());
        assert!(second_a.exists() && unrelated.exists());
        drop(second);
        assert!(!second_a.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn stale_sweep_preserves_live_unknown_and_unrelated_tasks() {
        let directory = tempfile::tempdir().unwrap();
        let boot = Uuid::new_v4();
        let marker = |owner_boot: Uuid, pid, start| {
            let path = directory.path().join(format!(
                "{PREFIX}{}-{pid}-{start}-{}-{}",
                owner_boot.simple(),
                Uuid::new_v4().simple(),
                Uuid::new_v4().simple(),
            ));
            fs::write(&path, "").unwrap();
            path
        };
        let live = marker(boot, 1, 100);
        let reused_pid = marker(boot, 1, 99);
        let dead = marker(boot, 2, 100);
        let unknown = marker(boot, 3, 100);
        let old_boot = marker(Uuid::new_v4(), 1, 100);
        let unrelated = directory.path().join("crew-turn-v1-not-owned");
        fs::write(&unrelated, "").unwrap();
        sweep_stale(directory.path(), boot, |pid| match pid {
            1 => Ok(100),
            2 => Err(io::ErrorKind::NotFound.into()),
            _ => Err(io::ErrorKind::PermissionDenied.into()),
        })
        .unwrap();
        assert!(live.exists() && unknown.exists() && unrelated.exists());
        assert!(!reused_pid.exists() && !dead.exists() && !old_boot.exists());
    }

    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use comet_harness::{Harness, HarnessError, RunControls};
    use comet_proto::{
        AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
        SessionStatus, SteeringMode,
    };
    use futures::{StreamExt, stream::BoxStream};
    use tokio::sync::mpsc;

    use crate::doc_host::{DocHost, DocHostConfig};
    use crate::registry::HarnessRegistry;
    use crate::run_journal::RunJournal;
    use crate::sessions::{QueueOutcome, SessionsEngine, lock};

    #[derive(Clone, Copy)]
    enum Startup {
        Ready,
        Fail,
        Wait,
    }

    struct Started {
        controls: RunControls,
        events: mpsc::UnboundedSender<Result<AgentEvent, HarnessError>>,
    }

    struct ActivityHarness {
        directory: PathBuf,
        started: mpsc::UnboundedSender<Started>,
        startup: Startup,
        mode: SteeringMode,
    }

    #[async_trait]
    impl Harness for ActivityHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "Namespace activity test"
        }
        fn supports_steering(&self) -> bool {
            true
        }
        fn steering_mode(&self) -> SteeringMode {
            self.mode
        }
        fn reasoning_levels(&self) -> &[ReasoningLevel] {
            &[]
        }
        async fn models(&self) -> Result<Vec<Model>, HarnessError> {
            Ok(Vec::new())
        }

        async fn run(
            &self,
            _request: RunRequest,
            controls: RunControls,
        ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
            assert!(
                !markers(&self.directory).is_empty(),
                "protection must precede harness startup"
            );
            let (events, rx) = mpsc::unbounded_channel();
            self.started
                .send(Started { controls, events })
                .unwrap_or_else(|_| panic!("test receiver closed"));
            match self.startup {
                Startup::Fail => Err(HarnessError::Protocol("startup failed".into())),
                Startup::Wait => std::future::pending().await,
                Startup::Ready => Ok(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|event| (event, rx))
                })
                .boxed()),
            }
        }
    }

    fn engine(
        root: &Path,
        directory: &Path,
        startup: Startup,
        mode: SteeringMode,
    ) -> (SessionsEngine, mpsc::UnboundedReceiver<Started>) {
        let (started, receiver) = mpsc::unbounded_channel();
        let registry = Arc::new(HarnessRegistry::for_profile(
            comet_proto::RuntimeProfile::Mock,
        ));
        registry.register(Arc::new(ActivityHarness {
            directory: directory.to_path_buf(),
            started,
            startup,
            mode,
        }));
        let mut sessions = SessionsEngine::new(
            "test-device".into(),
            Arc::new(RunJournal::open(root.join("journal")).unwrap()),
            registry,
            27654,
        );
        Arc::get_mut(&mut sessions.inner).unwrap().namespace_tasks = tasks_in(directory);
        sessions.set_doc_host(DocHost::new(
            Arc::new(comet_sync::DocsStore::open(root.join("docs")).unwrap()),
            DocHostConfig {
                device_id: "test-device".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        ));
        (sessions, receiver)
    }

    fn request() -> RunRequest {
        RunRequest {
            prompt: "work".into(),
            model: None,
            agent_account_id: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "/tmp".into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            attachments: Vec::new(),
            resume: Some("existing-native-session".into()),
        }
    }

    fn markers(directory: &Path) -> BTreeSet<PathBuf> {
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    fn done(status: DoneStatus) -> Result<AgentEvent, HarnessError> {
        Ok(AgentEvent::Done {
            status,
            result: None,
            error: None,
            session_id: None,
        })
    }

    async fn idle(sessions: &SessionsEngine, chat: &str) {
        let mut updates = sessions.watch_sessions();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if updates
                    .borrow_and_update()
                    .iter()
                    .any(|s| s.chat_id == chat && s.status == SessionStatus::Idle)
                {
                    break;
                }
                updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn namespace_turns_rotate_for_queued_and_steered_work_but_not_parked_children() {
        let root = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let (sessions, mut started) = engine(
            root.path(),
            directory.path(),
            Startup::Ready,
            SteeringMode::TurnBoundary,
        );
        sessions
            .dispatch("chat", HarnessId::Mock, request(), None)
            .await
            .unwrap();
        let mut run = started.recv().await.unwrap();
        let first = markers(directory.path());
        assert_eq!(first.len(), 1);
        assert_eq!(
            sessions.queue("chat", "queued", None).await.unwrap(),
            QueueOutcome::Queued
        );
        assert_eq!(markers(directory.path()), first);
        run.events.send(done(DoneStatus::Completed)).unwrap();
        assert_eq!(run.controls.steering.recv().await.unwrap().prompt, "queued");
        let queued = markers(directory.path());
        assert_eq!(queued.len(), 1);
        assert_ne!(queued, first);
        run.events.send(done(DoneStatus::Completed)).unwrap();
        idle(&sessions, "chat").await;
        assert!(markers(directory.path()).is_empty());

        // A resumed persistent process is protected before its mailbox sees work.
        sessions
            .dispatch("chat", HarnessId::Mock, request(), None)
            .await
            .unwrap();
        run.controls.steering.recv().await.unwrap();
        let active = markers(directory.path());
        assert_eq!(active.len(), 1);
        sessions.steer("chat", "next boundary", None).await.unwrap();
        run.controls.steering.recv().await.unwrap();
        assert_eq!(markers(directory.path()), active);
        run.events.send(done(DoneStatus::Completed)).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // The run lock spans marker rotation, so no transient gap is observed.
                let rotated = {
                    let _runs = lock(&sessions.inner.runs);
                    let current = markers(directory.path());
                    current.len() == 1 && current != active
                };
                if rotated {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        run.events
            .send(Ok(AgentEvent::Steered {
                assistant_message_id: None,
                next_assistant_message_id: None,
            }))
            .unwrap();
        run.events.send(done(DoneStatus::Completed)).unwrap();
        idle(&sessions, "chat").await;
        assert!(markers(directory.path()).is_empty());
        assert_eq!(
            sessions.queue("chat", "from idle", None).await.unwrap(),
            QueueOutcome::Delivered
        );
        run.controls.steering.recv().await.unwrap();
        assert_eq!(markers(directory.path()).len(), 1);
        run.events
            .send(Err(HarnessError::Protocol("stream failed".into())))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !lock(&sessions.inner.runs).is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(markers(directory.path()).is_empty());
        sessions.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn namespace_stop_is_bounded_and_preserves_other_active_sessions() {
        let root = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let (sessions, mut started) = engine(
            root.path(),
            directory.path(),
            Startup::Ready,
            SteeringMode::StepBoundary,
        );
        sessions
            .dispatch("a", HarnessId::Mock, request(), None)
            .await
            .unwrap();
        let first = started.recv().await.unwrap();
        let first_paths = markers(directory.path());
        sessions
            .dispatch("b", HarnessId::Mock, request(), None)
            .await
            .unwrap();
        let second = started.recv().await.unwrap();
        let second_paths: BTreeSet<_> = markers(directory.path())
            .difference(&first_paths)
            .cloned()
            .collect();
        sessions.interrupt("a").await.unwrap(); // Harness deliberately ignores cancellation.
        assert!(first.controls.interrupt.is_cancelled());
        assert_eq!(markers(directory.path()), second_paths);
        assert!(!second.controls.interrupt.is_cancelled());
        sessions.shutdown_now();
        assert!(second.controls.interrupt.is_cancelled());
        assert!(markers(directory.path()).is_empty());
        assert!(
            sessions
                .dispatch("c", HarnessId::Mock, request(), None)
                .await
                .is_err()
        );
        sessions.shutdown().await;
    }

    #[tokio::test]
    async fn namespace_failed_start_and_aborted_tasks_release_markers() {
        let root = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let (sessions, mut started) = engine(
            root.path(),
            directory.path(),
            Startup::Fail,
            SteeringMode::StepBoundary,
        );
        let error = sessions
            .dispatch("chat", HarnessId::Mock, request(), None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::EngineError::Harness(HarnessError::Protocol(_))
        ));
        assert!(
            started
                .try_recv()
                .unwrap()
                .controls
                .interrupt
                .is_cancelled()
        );
        assert!(markers(directory.path()).is_empty());
        sessions.shutdown().await;

        let other_root = tempfile::tempdir().unwrap();
        let (sessions, mut started) = engine(
            other_root.path(),
            directory.path(),
            Startup::Wait,
            SteeringMode::StepBoundary,
        );
        let dispatch = sessions.clone();
        let task = tokio::spawn(async move {
            dispatch
                .dispatch("chat", HarnessId::Mock, request(), None)
                .await
        });
        let run = started.recv().await.unwrap();
        assert_eq!(markers(directory.path()).len(), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(run.controls.interrupt.is_cancelled());
        assert!(markers(directory.path()).is_empty());
        assert!(lock(&sessions.inner.runs).is_empty());
        sessions.shutdown().await;

        let streaming_root = tempfile::tempdir().unwrap();
        let (sessions, mut started) = engine(
            streaming_root.path(),
            directory.path(),
            Startup::Ready,
            SteeringMode::StepBoundary,
        );
        sessions
            .dispatch("chat", HarnessId::Mock, request(), None)
            .await
            .unwrap();
        let run = started.recv().await.unwrap();
        assert_eq!(markers(directory.path()).len(), 1);
        let tasks = std::mem::take(&mut *lock(&sessions.inner.run_tasks));
        for task in tasks {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        assert!(run.controls.interrupt.is_cancelled());
        assert!(markers(directory.path()).is_empty());
        assert!(lock(&sessions.inner.runs).is_empty());
        sessions.shutdown().await;
    }

    #[tokio::test]
    async fn namespace_unavailable_marker_directory_prevents_unprotected_execution() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let (sessions, mut started) = engine(
            root.path(),
            &missing,
            Startup::Ready,
            SteeringMode::StepBoundary,
        );
        assert!(
            sessions
                .dispatch("chat", HarnessId::Mock, request(), None)
                .await
                .is_err()
        );
        assert!(
            started.try_recv().is_err(),
            "harness must not execute without a marker"
        );
        assert!(lock(&sessions.inner.runs).is_empty());
        sessions.shutdown().await;
    }
}
