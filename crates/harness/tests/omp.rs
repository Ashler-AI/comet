use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}
use std::time::Duration;

use comet_harness::{CancellationToken, Harness, OmpHarness, RunControls};
use comet_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel};
use futures::StreamExt as _;
use tokio::sync::{mpsc, oneshot};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-omp.sh")
}

fn controls() -> RunControls {
    let (_steer_tx, steer_rx) = mpsc::channel(4);
    RunControls {
        request_input: Box::new(|_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        interrupt: CancellationToken::new(),
    }
}

fn request(resume: Option<&str>) -> RunRequest {
    RunRequest {
        prompt: "hello".into(),
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::DangerFullAccess,
        auto_approve: true,
        resume: resume.map(str::to_owned),
        attachments: Vec::new(),
    }
}

#[tokio::test]
async fn fake_omp_proves_acp_only_execution_and_event_mapping() {
    let _env = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
    }
    let harness = OmpHarness::new().with_executable(fixture_path());
    let stream = harness
        .run(request(None), controls())
        .await
        .expect("run starts");
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid ACP event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes");

    let argv = std::fs::read_to_string(argv_log).unwrap();
    assert_eq!(argv.lines().next(), Some("acp"));
    for forbidden in ["claude", "codex", "opencode"] {
        assert!(
            !argv.lines().any(|arg| arg == forbidden),
            "spawned forbidden harness: {argv}"
        );
    }
    assert!(events.contains(&AgentEvent::SessionStarted {
        harness: HarnessId::Omp,
        model: "default".into(),
        tools: vec![],
        cwd: String::new(),
        session_id: "omp-session-1".into(),
        assistant_message_id: "omp-omp-session-1".into(),
    }));
    assert!(
        events.contains(&AgentEvent::ReasoningDelta {
            text: "thinking".into()
        }),
        "events: {events:?}"
    );
    assert!(
        events.contains(&AgentEvent::TextDelta {
            text: "hello from omp".into()
        }),
        "events: {events:?}"
    );
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "tool-1".into(),
        is_error: false
    }));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("omp-session-1".into()),
        })
    );
}

#[tokio::test]
async fn resume_uses_the_requested_acp_session_id() {
    let _env = env_lock();
    let temp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv"));
    }
    let harness = OmpHarness::new().with_executable(fixture_path());
    let stream = harness
        .run(request(Some("resume-session")), controls())
        .await
        .expect("resume starts");
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid ACP event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes");
    assert!(
        events.iter().any(|event| matches!(event,
            AgentEvent::SessionStarted { session_id, .. } if session_id == "resume-session"
        )),
        "events: {events:?}"
    );
}
