use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard};

use comet_harness::{CancellationToken, Harness, PrimeAgentHarness, RunControls, SteerMessage};
use comet_proto::{AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel};
use futures::StreamExt as _;
use tokio::sync::{mpsc, oneshot};

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

async fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}

struct EnvGuard {
    name: &'static str,
    original: Option<OsString>,
}

impl EnvGuard {
    fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
        let original = std::env::var_os(name);
        unsafe { std::env::set_var(name, value.as_ref()) };
        Self { name, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.original.take() {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-prime-agent.sh")
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
        context: None,
    }
}

fn request(resume: Option<&str>) -> RunRequest {
    RunRequest {
        prompt: "hello".into(),
        model: Some("openai-codex/gpt-5.6-sol".into()),
        agent_account_id: None,
        reasoning: Some(ReasoningLevel::High),
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::DangerFullAccess,
        auto_approve: true,
        resume: resume.map(str::to_owned),
        attachments: Vec::new(),
    }
}

#[tokio::test]
async fn exposes_prime_model_and_command_catalogs() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", temp.path().join("argv"));
    let harness = PrimeAgentHarness::new().with_executable(fixture_path());

    let models = harness.models().await.expect("model catalog");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "openai-codex/gpt-5.6-sol");
    assert_eq!(
        models[0].reasoning_levels.last(),
        Some(&ReasoningLevel::Max)
    );

    let commands = harness.commands("").await.expect("command catalog");
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].name, "session_name");
    assert_eq!(commands[1].name, "skill:review");
}

#[tokio::test]
async fn new_prime_run_reports_a_durable_native_session_and_maps_events() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", temp.path().join("argv"));
    let _coding_agent_dir =
        EnvGuard::set("PRIME_AGENT_CODING_AGENT_DIR", temp.path().join("config"));
    let harness = PrimeAgentHarness::new().with_executable(fixture_path());
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

    let session_id = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::SessionStarted {
                harness: HarnessId::PrimeAgent,
                session_id,
                ..
            } => Some(session_id),
            _ => None,
        })
        .expect("Prime session started");
    assert!(session_id.ends_with("native-prime-session.jsonl"));
    assert!(PathBuf::from(session_id).is_file());
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "hello from prime".into(),
    }));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some(session_id.clone()),
        })
    );

    let argv = std::fs::read_to_string(temp.path().join("argv")).unwrap();
    assert!(argv.lines().any(|arg| arg == "--session-dir"));
    assert!(argv.lines().any(|arg| arg == "--model"));
    assert!(argv.lines().any(|arg| arg == "openai-codex/gpt-5.6-sol"));
    assert!(argv.lines().any(|arg| arg == "--thinking"));
    assert!(argv.lines().any(|arg| arg == "high"));
    assert!(
        argv.lines()
            .any(|argument| argument == "--dangerously-skip-permissions")
    );
}

#[tokio::test]
async fn prime_resume_uses_the_native_token_without_creating_a_parallel_store() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let resume_log = temp.path().join("resume");
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", temp.path().join("argv"));
    let _resume_log = EnvGuard::set("PRIME_RESUME_LOG", &resume_log);
    let _coding_agent_dir =
        EnvGuard::set("PRIME_AGENT_CODING_AGENT_DIR", temp.path().join("config"));
    let harness = PrimeAgentHarness::new().with_executable(fixture_path());
    let stream = harness
        .run(request(Some("native-prime-session")), controls())
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

    assert_eq!(
        std::fs::read_to_string(resume_log).unwrap(),
        "native-prime-session"
    );
    let argv = std::fs::read_to_string(temp.path().join("argv")).unwrap();
    assert!(argv.lines().any(|arg| arg == "--resume"));
    assert!(!argv.lines().any(|arg| arg == "--session-dir"));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SessionStarted { session_id, .. } if session_id == "native-prime-session"
    )));
}

#[tokio::test]
async fn persistent_prime_run_delivers_queued_turn_boundary_steers() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let prompt_log = temp.path().join("prompts");
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", temp.path().join("argv"));
    let _prompt_log = EnvGuard::set("PRIME_PROMPT_LOG", &prompt_log);
    let _keep_open = EnvGuard::set("PRIME_KEEP_OPEN", "1");
    let _coding_agent_dir =
        EnvGuard::set("PRIME_AGENT_CODING_AGENT_DIR", temp.path().join("config"));
    let harness = PrimeAgentHarness::new().with_executable(fixture_path());
    let (steer_tx, steer_rx) = mpsc::channel(4);
    let controls = RunControls {
        request_input: Box::new(|_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        interrupt: CancellationToken::new(),
        context: None,
    };
    let mut stream = harness
        .run(request(None), controls)
        .await
        .expect("persistent run starts");
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("first turn completes")
            .expect("stream remains open")
            .expect("valid first-turn event");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }

    steer_tx
        .send(SteerMessage {
            prompt: "follow up".into(),
            message_id: Some("user-follow-up".into()),
        })
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("second turn completes")
            .expect("stream remains open")
            .expect("valid second-turn event");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }
    drop(steer_tx);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("mailbox closure reaps ACP child")
            .is_none()
    );

    let first_done = events
        .iter()
        .position(|event| matches!(event, AgentEvent::Done { .. }))
        .unwrap();
    let boundary = events
        .iter()
        .position(|event| matches!(event, AgentEvent::Steered { .. }))
        .unwrap();
    let second_done = events
        .iter()
        .rposition(|event| matches!(event, AgentEvent::Done { .. }))
        .unwrap();
    assert!(first_done < boundary && boundary < second_done);
    assert_eq!(
        std::fs::read_to_string(prompt_log).unwrap().lines().count(),
        2
    );
}

#[tokio::test(flavor = "current_thread")]
async fn active_prime_tool_survives_the_prompt_inactivity_deadline() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let gate = temp.path().join("release-long-tool");
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", temp.path().join("argv"));
    let _coding_agent_dir =
        EnvGuard::set("PRIME_AGENT_CODING_AGENT_DIR", temp.path().join("config"));
    let _long_tool_gate = EnvGuard::set("PRIME_LONG_TOOL_GATE", &gate);
    let harness = PrimeAgentHarness::new().with_executable(fixture_path());
    let mut stream = harness
        .run(request(None), controls())
        .await
        .expect("run starts");

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = stream
                .next()
                .await
                .expect("stream remains open")
                .expect("valid ACP event");
            if matches!(event, AgentEvent::ToolCall { ref id, .. } if id == "long-tool") {
                break;
            }
        }
    })
    .await
    .expect("long tool starts");

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(301)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();

    std::fs::write(gate, b"done").unwrap();
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid ACP event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes after the inactivity deadline");

    assert!(events.contains(&AgentEvent::ToolResult {
        id: "long-tool".into(),
        is_error: false,
        output: None,
    }));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn completed_prime_tool_can_be_followed_by_prompt_silence() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let gate = temp.path().join("release-post-tool-silence");
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", temp.path().join("argv"));
    let _coding_agent_dir =
        EnvGuard::set("PRIME_AGENT_CODING_AGENT_DIR", temp.path().join("config"));
    let _post_tool_gate = EnvGuard::set("PRIME_POST_TOOL_GATE", &gate);
    let harness = PrimeAgentHarness::new().with_executable(fixture_path());
    let mut stream = harness
        .run(request(None), controls())
        .await
        .expect("run starts");

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = stream
                .next()
                .await
                .expect("stream remains open")
                .expect("valid ACP event");
            if matches!(
                event,
                AgentEvent::ToolResult {
                    ref id,
                    is_error: false,
                    ..
                } if id == "completed-tool"
            ) {
                break;
            }
        }
    })
    .await
    .expect("tool completes before the silent phase");

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(301)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();

    std::fs::write(gate, b"done").unwrap();
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid ACP event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes after post-tool inactivity");

    assert!(events.contains(&AgentEvent::TextDelta {
        text: "hello from prime".into(),
    }));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}
