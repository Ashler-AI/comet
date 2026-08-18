use std::path::PathBuf;
use tokio::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

async fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}
use std::time::Duration;

use comet_harness::{CancellationToken, Harness, OmpHarness, RunControls, SteerMessage};
use comet_proto::{AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel};
use futures::StreamExt as _;
use tokio::sync::{mpsc, oneshot};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-omp.sh")
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
fn run_config_path(session_log: &str) -> PathBuf {
    let value = session_log
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("config:"))
        .expect("fixture config environment");
    std::env::split_paths(value)
        .last()
        .expect("Comet retry overlay path")
}

fn write_omp_session(session_dir: &std::path::Path, session_id: &str) -> PathBuf {
    let project_dir = session_dir.join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let path = project_dir.join("session.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "session",
                "id": session_id,
                "cwd": "/tmp",
            })
        ),
    )
    .unwrap();
    path
}

#[cfg(unix)]
#[test]
#[ignore = "subprocess helper for verified writer takeover"]
fn omp_writer_process_helper() {
    let Some(path) = std::env::var_os("COMET_TEST_OMP_WRITER_PATH") else {
        return;
    };
    let ready = std::env::var_os("COMET_TEST_OMP_WRITER_READY").unwrap();
    let _journal = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open test journal for append");
    std::fs::write(ready, std::process::id().to_string()).unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(unix)]
#[test]
#[ignore = "subprocess helper for orphaned writer takeover"]
fn orphaned_omp_writer_process_helper() {
    let Some(path) = std::env::var_os("COMET_TEST_OMP_WRITER_PATH") else {
        return;
    };
    let ready = std::env::var_os("COMET_TEST_OMP_WRITER_READY").unwrap();
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exec 3>>\"$COMET_TEST_OMP_WRITER_PATH\"; echo $$ >\"$COMET_TEST_OMP_WRITER_READY\"; exec /bin/sleep 600")
        .env("COMET_TEST_OMP_WRITER_PATH", path)
        .env("COMET_TEST_OMP_WRITER_READY", ready)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn orphaned writer");
    // The outer test launched this helper into an isolated process group. Exit
    // now so the write-capable tool is reparented to PID 1, matching the real
    // agent-browser zombie left after OMP died.
    std::process::exit(0);
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
        model: None,
        agent_account_id: None,
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
async fn model_and_command_catalogs_come_from_omp() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv"));
    }
    let harness = OmpHarness::new().with_executable(fixture_path());
    let models = harness.models().await.expect("model catalog");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "openai-codex/gpt-5.6-sol");
    assert_eq!(
        models[0].reasoning_levels,
        vec![
            ReasoningLevel::Low,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
        ]
    );

    let commands = harness.commands("").await.expect("command catalog");
    assert_eq!(commands.len(), 3);
    assert_eq!(commands[0].name, "ralplan");
    assert_eq!(commands[0].input_hint.as_deref(), Some("goal"));
    assert_eq!(commands[1].name, "security");
    assert_eq!(commands[2].name, "goal");
    assert_eq!(
        commands[2]
            .subcommands
            .iter()
            .map(|subcommand| subcommand.name.as_str())
            .collect::<Vec<_>>(),
        ["set", "show", "pause", "resume", "drop", "budget"]
    );
}

#[tokio::test]
async fn goal_command_ack_refreshes_state_without_goal_updated() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv"));
        std::env::set_var("OMP_OMIT_GOAL_EVENTS", "1");
    }
    let harness = OmpHarness::new().with_executable(fixture_path());
    let mut run_request = request(None);
    run_request.prompt = "/goal set Persistent editor indicator".into();
    let stream = harness
        .run(run_request, controls())
        .await
        .expect("run starts");
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid RPC event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("goal command completes");

    let goal = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolCall {
                id,
                call:
                    comet_proto::ToolCall::Unknown {
                        input: Some(input), ..
                    },
            } if id == comet_proto::OMP_GOAL_STATE_CALL_ID => input.get("goal"),
            _ => None,
        })
        .next_back()
        .expect("refreshed goal state");
    assert_eq!(
        goal.get("objective").and_then(serde_json::Value::as_str),
        Some("Persistent editor indicator")
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));
    unsafe {
        std::env::remove_var("OMP_OMIT_GOAL_EVENTS");
    }
}

#[tokio::test]
async fn fake_omp_proves_rpc_mode_execution_and_event_mapping() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv");
    let session_log = temp.path().join("sessions");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
        std::env::set_var("OMP_SESSION_LOG", &session_log);
    }
    let harness = OmpHarness::new().with_executable(fixture_path());
    let mut run_request = request(None);
    run_request.auto_approve = false;
    let stream = harness
        .run(run_request, controls())
        .await
        .expect("run starts");
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid RPC event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes");

    let argv = std::fs::read_to_string(argv_log).unwrap();
    let mut lines = argv.lines();
    assert_eq!(lines.next(), Some("--mode"));
    assert_eq!(lines.next(), Some("rpc"));
    assert!(
        argv.contains("--approval-mode\nyolo\n"),
        "Comet OMP runs must explicitly select yolo approval mode: {argv}"
    );
    for forbidden in ["claude", "codex", "opencode"] {
        assert!(
            !argv.lines().any(|arg| arg == forbidden),
            "spawned forbidden harness: {argv}"
        );
    }
    assert!(
        !argv.lines().any(|argument| argument == "--config"),
        "Comet should use the process-only PI_CONFIG_FILES interface: {argv}"
    );
    let session_log = std::fs::read_to_string(session_log).unwrap();
    let run_config = run_config_path(&session_log);
    assert!(
        run_config.is_absolute(),
        "overlay path must be absolute: {session_log}"
    );
    assert_eq!(session_log.lines().nth(1), Some("get_state"));
    assert!(
        !run_config.exists(),
        "run overlay must be removed after OMP exits: {}",
        run_config.display()
    );
    let assistant_message_id = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::SessionStarted {
                harness: HarnessId::Omp,
                model,
                tools,
                cwd,
                session_id,
                assistant_message_id,
            } if model == "openai-codex/gpt-5.6-sol"
                && tools.is_empty()
                && cwd.is_empty()
                && session_id == "omp-session-1" =>
            {
                Some(assistant_message_id)
            }
            _ => None,
        })
        .expect("OMP session start");
    assert!(assistant_message_id.starts_with("omp-rpc-omp-session-1-"));
    assert!(assistant_message_id.ends_with("-0"));
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
    assert!(
        events.contains(&AgentEvent::ToolCall {
            id: "tool-1".into(),
            call: comet_proto::ToolCall::ReadFile {
                path: "README.md".into()
            },
        }),
        "RPC tool names must map to native call kinds: {events:?}"
    );
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "tool-1".into(),
        is_error: false,
        output: Some("# README".into()),
    }));
    assert!(
        events.contains(&AgentEvent::Usage {
            input_tokens: 106,
            output_tokens: 4,
        }),
        "assistant message_end usage must surface: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("omp-session-1".into()),
        })
    );
    unsafe {
        std::env::remove_var("OMP_SESSION_LOG");
    }
}

#[tokio::test]
async fn rpc_ui_dialogs_round_trip_through_shared_input() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv"));
    }
    let (asked_tx, asked_rx) = std::sync::mpsc::channel();
    let (steer_tx, steering) = mpsc::channel(4);
    drop(steer_tx);
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let _ = asked_tx.send(questions.clone());
            let answers = questions
                .iter()
                .map(|question| comet_proto::UserInputAnswer {
                    question_id: question.id.clone(),
                    labels: vec![question.options[0].clone()],
                })
                .collect();
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(answers);
            rx
        }),
        steering,
        interrupt: CancellationToken::new(),
        context: None,
    };
    let harness = OmpHarness::new().with_executable(fixture_path());
    let mut run_request = request(None);
    run_request.prompt = "scenario:elicitation".into();
    let stream = harness
        .run(run_request, controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid RPC event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes");

    let asked = [
        asked_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        asked_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
    ]
    .concat();
    assert_eq!(asked.len(), 2, "{events:?}");
    assert_eq!(asked[0].header, "Provider connection");
    assert_eq!(asked[0].options, ["Continue", "Cancel"]);
    assert_eq!(asked[1].header, "Region");
    assert_eq!(asked[1].options, ["East", "West"]);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn selected_model_and_reasoning_ride_the_spawn_flags() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
    }
    let harness = OmpHarness::new().with_executable(fixture_path());
    let mut request = request(None);
    request.model = Some("openrouter/~deepseek/deepseek-v4-flash-latest".into());
    request.reasoning = Some(ReasoningLevel::XHigh);
    let stream = harness.run(request, controls()).await.expect("run starts");
    tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid RPC event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes");

    let argv = std::fs::read_to_string(argv_log).unwrap();
    assert!(
        argv.contains("--model\nopenrouter/~deepseek/deepseek-v4-flash-latest\n"),
        "model must be pinned at spawn: {argv}"
    );
    assert!(
        argv.contains("--thinking\nxhigh\n"),
        "thinking level must be pinned at spawn: {argv}"
    );
}

#[tokio::test]
async fn resume_pins_the_requested_session_at_spawn() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv");
    let session_dir = temp.path().join("sessions");
    write_omp_session(&session_dir, "resume-session");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
        std::env::set_var("OMP_WRITER_STATE", "inactive");
    }
    let harness = OmpHarness::new()
        .with_executable(fixture_path())
        .with_session_dir(session_dir)
        .with_session_writer_probe(fixture_path());
    let stream = harness
        .run(request(Some("resume-session")), controls())
        .await
        .expect("resume starts");
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid RPC event"))
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
    let argv = std::fs::read_to_string(argv_log).unwrap();
    assert!(
        argv.contains("--resume\nresume-session\n"),
        "resume must be pinned at spawn: {argv}"
    );
    unsafe {
        std::env::remove_var("OMP_WRITER_STATE");
    }
}

#[tokio::test]
async fn active_resume_is_rejected_before_a_second_omp_process_starts() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv");
    let session_dir = temp.path().join("sessions");
    write_omp_session(&session_dir, "active-session");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
        std::env::set_var("OMP_WRITER_STATE", "active");
    }
    let harness = OmpHarness::new()
        .with_executable(fixture_path())
        .with_session_dir(session_dir)
        .with_session_writer_probe(fixture_path());
    let error = match harness
        .run(request(Some("active-session")), controls())
        .await
    {
        Ok(_) => panic!("active OMP session started a second writer"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("already running"),
        "unexpected error: {error}"
    );
    assert!(
        !argv_log.exists(),
        "the OMP executable must not start while another writer is active"
    );
    unsafe {
        std::env::remove_var("OMP_WRITER_STATE");
    }
}

#[tokio::test]
async fn hung_abort_is_force_killed_before_the_session_can_resume() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv");
    let pid_log = temp.path().join("pid");
    let session_dir = temp.path().join("sessions");
    write_omp_session(&session_dir, "hung-session");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
        std::env::set_var("OMP_RPC_PID_LOG", &pid_log);
        std::env::set_var("OMP_WRITER_STATE", "auto");
        std::env::set_var("OMP_STEER_SCENARIO", "1");
        std::env::set_var("OMP_HANG_ABORT", "1");
    }
    let harness = OmpHarness::new()
        .with_executable(fixture_path())
        .with_session_dir(&session_dir)
        .with_session_writer_probe(fixture_path())
        .with_interrupt_grace(Duration::from_millis(50));
    let interrupt = CancellationToken::new();
    let (_steer_tx, steering) = mpsc::channel(4);
    let run_controls = RunControls {
        request_input: Box::new(|_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering,
        interrupt: interrupt.clone(),
        context: None,
    };
    let mut stream = harness
        .run(request(Some("hung-session")), run_controls)
        .await
        .expect("first resume starts");
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("session starts")
            .expect("stream remains open")
            .expect("valid RPC event");
        if matches!(event, AgentEvent::SessionStarted { .. }) {
            break;
        }
    }

    let error = match harness.run(request(Some("hung-session")), controls()).await {
        Ok(_) => panic!("active OMP session started a second writer"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("already running"));

    interrupt.cancel();
    let events = tokio::time::timeout(
        Duration::from_secs(2),
        stream
            .map(|event| event.expect("valid interrupted event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("hung abort is force-killed within the interrupt deadline");
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Interrupted,
            ..
        })
    ));

    unsafe {
        std::env::remove_var("OMP_HANG_ABORT");
        std::env::remove_var("OMP_STEER_SCENARIO");
    }
    let resumed = harness
        .run(request(Some("hung-session")), controls())
        .await
        .expect("session resumes after interrupt teardown");
    let resumed_events = tokio::time::timeout(
        Duration::from_secs(10),
        resumed
            .map(|event| event.expect("valid resumed event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("resumed turn completes");
    assert!(matches!(
        resumed_events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
    unsafe {
        std::env::remove_var("OMP_RPC_PID_LOG");
        std::env::remove_var("OMP_WRITER_STATE");
    }
}

#[tokio::test]
async fn errored_rpc_run_releases_writer_before_done() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv");
    let pid_log = temp.path().join("pid");
    let session_dir = temp.path().join("sessions");
    write_omp_session(&session_dir, "errored-session");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
        std::env::set_var("OMP_RPC_PID_LOG", &pid_log);
        std::env::set_var("OMP_WRITER_STATE", "auto");
        std::env::set_var("OMP_TURN_ERROR", "1");
    }
    let harness = OmpHarness::new()
        .with_executable(fixture_path())
        .with_session_dir(&session_dir)
        .with_session_writer_probe(fixture_path());
    let mut stream = harness
        .run(request(Some("errored-session")), controls())
        .await
        .expect("errored turn starts");
    let mut saw_provider_error = false;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("errored turn settles")
            .expect("stream remains open")
            .expect("valid errored event");
        match event {
            AgentEvent::Error { message } => {
                saw_provider_error = message.contains("No API key for provider");
            }
            AgentEvent::Done { status, .. } => {
                assert_eq!(status, DoneStatus::Errored);
                assert!(saw_provider_error);
                #[cfg(unix)]
                {
                    let pid = std::fs::read_to_string(&pid_log)
                        .expect("fixture recorded the OMP pid")
                        .trim()
                        .parse::<i32>()
                        .expect("recorded OMP pid is numeric");
                    assert!(
                        !process_exists(pid),
                        "errored Done became visible while OMP pid {pid} still owned the session"
                    );
                }
                break;
            }
            _ => {}
        }
    }

    unsafe {
        std::env::remove_var("OMP_TURN_ERROR");
    }
    let resumed = harness
        .run(request(Some("errored-session")), controls())
        .await
        .expect("session resumes as soon as errored Done is visible");
    let resumed_events = tokio::time::timeout(
        Duration::from_secs(10),
        resumed
            .map(|event| event.expect("valid resumed event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("resumed turn completes");
    assert!(matches!(
        resumed_events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
    unsafe {
        std::env::remove_var("OMP_RPC_PID_LOG");
        std::env::remove_var("OMP_WRITER_STATE");
    }
}

#[tokio::test]
async fn unknown_writer_state_fails_closed_before_spawning_omp() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv");
    let session_dir = temp.path().join("sessions");
    write_omp_session(&session_dir, "unknown-session");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
        std::env::set_var("OMP_WRITER_STATE", "unknown");
    }
    let harness = OmpHarness::new()
        .with_executable(fixture_path())
        .with_session_dir(session_dir)
        .with_session_writer_probe(fixture_path());
    let error = match harness
        .run(request(Some("unknown-session")), controls())
        .await
    {
        Ok(_) => panic!("unverified OMP session started a second writer"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("Could not verify"),
        "unexpected error: {error}"
    );
    assert!(
        !argv_log.exists(),
        "OMP must not start when ownership is unknown"
    );
    unsafe {
        std::env::remove_var("OMP_WRITER_STATE");
    }
}

#[tokio::test]
async fn missing_writer_probe_fails_closed_before_spawning_omp() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv");
    let session_dir = temp.path().join("sessions");
    write_omp_session(&session_dir, "missing-probe-session");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
    }
    let harness = OmpHarness::new()
        .with_executable(fixture_path())
        .with_session_dir(session_dir)
        .with_session_writer_probe(temp.path().join("does-not-exist"));
    let error = match harness
        .run(request(Some("missing-probe-session")), controls())
        .await
    {
        Ok(_) => panic!("OMP started without an ownership probe"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("Could not verify"),
        "unexpected error: {error}"
    );
    assert!(
        !argv_log.exists(),
        "OMP must not start when the ownership probe is unavailable"
    );
}

#[tokio::test]
async fn persistent_run_steers_the_live_turn() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let prompt_log = temp.path().join("prompts");
    let argv_log = temp.path().join("argv");
    let session_log = temp.path().join("sessions");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
        std::env::set_var("OMP_PROMPT_LOG", &prompt_log);
        std::env::set_var("OMP_SESSION_LOG", &session_log);
        std::env::set_var("OMP_STEER_SCENARIO", "1");
    }
    let harness = OmpHarness::new().with_executable(fixture_path());
    assert!(harness.supports_steering());
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
    for _ in 0..100 {
        if prompt_log.is_file() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(prompt_log.is_file(), "first RPC prompt did not start");
    steer_tx
        .send(SteerMessage {
            prompt: "follow up".into(),
            message_id: Some("user-follow-up".into()),
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("steered turn completes")
            .expect("stream remains open")
            .expect("valid steered-turn event");
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
            .expect("mailbox closure reaps the RPC child")
            .is_none()
    );

    // The steer altered the LIVE turn: exactly one Done, with the steering
    // boundary and the steered output landing before it — not queued behind it.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::Done { .. }))
            .count(),
        1,
        "a mid-turn steer must not split the turn: {events:?}"
    );
    let boundary = events
        .iter()
        .position(|event| matches!(event, AgentEvent::Steered { .. }))
        .expect("steering boundary");
    let steered_output = events
        .iter()
        .position(
            |event| matches!(event, AgentEvent::TextDelta { text } if text == "steered reply"),
        )
        .expect("steered output");
    let done = events
        .iter()
        .position(|event| matches!(event, AgentEvent::Done { .. }))
        .unwrap();
    assert!(
        boundary < steered_output && steered_output < done,
        "events: {events:?}"
    );
    let session_log = std::fs::read_to_string(session_log).unwrap();
    assert_eq!(
        session_log.lines().nth(1),
        Some("get_state"),
        "one RPC session must serve the whole run without mutating OMP settings"
    );
    let argv = std::fs::read_to_string(argv_log).unwrap();
    assert!(
        !argv.lines().any(|argument| argument == "--config"),
        "persistent runs should use PI_CONFIG_FILES: {argv}"
    );
    let run_config = run_config_path(&session_log);
    assert!(
        !run_config.exists(),
        "persistent run overlay must be removed after OMP exits: {}",
        run_config.display()
    );
    let prompts = std::fs::read_to_string(prompt_log).unwrap();
    assert_eq!(prompts.lines().count(), 2);
    let steer_line = prompts.lines().nth(1).unwrap();
    assert!(steer_line.contains("follow up"));
    assert!(
        steer_line.contains("\"streamingBehavior\":\"steer\""),
        "steers must ride the steering path: {steer_line}"
    );
    unsafe {
        std::env::remove_var("OMP_PROMPT_LOG");
        std::env::remove_var("OMP_SESSION_LOG");
        std::env::remove_var("OMP_STEER_SCENARIO");
    }
}

#[tokio::test]
async fn rpc_error_details_reach_the_harness_error() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv"));
        std::env::set_var(
            "OMP_PROMPT_ERROR_DETAILS",
            "openai-codex/gpt-5.6-sol billing limit reached",
        );
    }
    let harness = OmpHarness::new().with_executable(fixture_path());
    let mut stream = harness
        .run(request(None), controls())
        .await
        .expect("run starts");
    let error = loop {
        match tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("RPC error arrives")
        {
            Some(Err(error)) => break error,
            Some(Ok(_)) => {}
            None => panic!("stream closed without the RPC error"),
        }
    };
    let message = error.to_string();
    assert!(message.contains("openai-codex/gpt-5.6-sol"));
    assert!(message.contains("billing limit reached"));
    assert!(!message.ends_with("Internal error"));
    unsafe { std::env::remove_var("OMP_PROMPT_ERROR_DETAILS") };
}

#[cfg(unix)]
#[tokio::test]
async fn verified_takeover_stops_exact_writer_and_releases_journal() {
    use std::os::unix::process::CommandExt as _;

    let temp = tempfile::tempdir().unwrap();
    let session_dir = temp.path().join("sessions");
    let journal = write_omp_session(&session_dir, "takeover-session");
    let ready = temp.path().join("writer-ready");
    let executable = std::env::current_exe().unwrap();
    let mut command = std::process::Command::new(&executable);
    command
        .args([
            "--ignored",
            "--exact",
            "omp_writer_process_helper",
            "--nocapture",
        ])
        .env("COMET_TEST_OMP_WRITER_PATH", &journal)
        .env("COMET_TEST_OMP_WRITER_READY", &ready)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !ready.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready.exists(), "writer helper did not start");
    assert_eq!(
        comet_harness::omp::session_writer_state(&journal),
        comet_harness::omp::SessionWriterState::Active
    );

    let result = OmpHarness::new()
        .with_executable(&executable)
        .with_session_dir(&session_dir)
        .stop_session("takeover-session")
        .await;
    if let Err(error) = result {
        let _ = child.kill();
        panic!("verified takeover failed: {error}");
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        child.try_wait().unwrap().is_some(),
        "writer process survived takeover"
    );
    assert_eq!(
        comet_harness::omp::session_writer_state(&journal),
        comet_harness::omp::SessionWriterState::Inactive
    );
}

#[cfg(unix)]
#[tokio::test]
async fn verified_takeover_stops_orphaned_tool_holding_the_journal() {
    use std::os::unix::process::CommandExt as _;

    let temp = tempfile::tempdir().unwrap();
    let session_dir = temp.path().join("sessions");
    let journal = write_omp_session(&session_dir, "orphaned-takeover-session");
    let ready = temp.path().join("orphaned-writer-ready");
    let executable = std::env::current_exe().unwrap();
    let mut launcher = std::process::Command::new(&executable)
        .args([
            "--ignored",
            "--exact",
            "orphaned_omp_writer_process_helper",
            "--nocapture",
        ])
        .env("COMET_TEST_OMP_WRITER_PATH", &journal)
        .env("COMET_TEST_OMP_WRITER_READY", &ready)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    assert!(launcher.wait().unwrap().success());

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !ready.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pid = std::fs::read_to_string(&ready)
        .expect("orphaned writer started")
        .trim()
        .parse::<i32>()
        .unwrap();
    assert!(process_exists(pid));
    assert_eq!(
        comet_harness::omp::session_writer_state(&journal),
        comet_harness::omp::SessionWriterState::Active
    );

    let result = OmpHarness::new()
        .with_executable(fixture_path())
        .with_session_dir(&session_dir)
        .stop_session("orphaned-takeover-session")
        .await;
    if let Err(error) = result {
        // SAFETY: the test created this exact process and recorded its PID.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        panic!("orphaned writer takeover failed: {error}");
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while process_exists(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!process_exists(pid), "orphaned tool survived takeover");
    assert_eq!(
        comet_harness::omp::session_writer_state(&journal),
        comet_harness::omp::SessionWriterState::Inactive
    );
}
#[cfg(unix)]
#[tokio::test]
async fn supervised_omp_run_is_its_process_group_leader() {
    use std::os::unix::fs::PermissionsExt as _;

    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let supervisor = temp.path().join("supervisor.sh");
    let group_log = temp.path().join("group");
    let argv_log = temp.path().join("argv");
    std::fs::write(
        &supervisor,
        "#!/bin/sh\nset -eu\n[ \"$1\" = \"__comet-omp-supervisor\" ]\nprintf '%s %s\\n' \"$$\" \"$(/bin/ps -o pgid= -p $$ | tr -d ' ')\" > \"$OMP_SUPERVISOR_GROUP_LOG\"\nexecutable=$3\nshift 3\nexec \"$executable\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&supervisor, std::fs::Permissions::from_mode(0o700)).unwrap();
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
        std::env::set_var("OMP_SUPERVISOR_GROUP_LOG", &group_log);
    }
    let stream = OmpHarness::new()
        .with_executable(fixture_path())
        .with_supervisor_executable(&supervisor)
        .run(request(None), controls())
        .await
        .expect("supervised OMP starts");
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.collect::<Vec<Result<AgentEvent, _>>>(),
    )
    .await
    .expect("supervised OMP settles");
    let ids = std::fs::read_to_string(&group_log)
        .unwrap()
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1], "supervisor must lead its own process group");
    unsafe {
        std::env::remove_var("OMP_SUPERVISOR_GROUP_LOG");
        std::env::remove_var("OMP_ARGV_LOG");
    }
}

/// Live smoke against the installed omp CLI: one streamed turn, a parked
/// followup as the next turn, and a clean mailbox-close teardown.
#[tokio::test]
#[ignore = "requires installed+authenticated omp CLI; spends tokens"]

async fn real_omp_streams_turns_over_rpc_mode() {
    let temp = tempfile::tempdir().unwrap();
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
    let mut run_request = request(None);
    run_request.prompt = "Reply with exactly: done".into();
    run_request.model = Some("anthropic/claude-haiku-4-5".into());
    run_request.cwd = temp.path().to_string_lossy().into_owned();
    let harness = OmpHarness::new();
    let mut stream = harness
        .run(run_request, controls)
        .await
        .expect("run starts");

    let mut session_id = None;
    let mut first_turn = String::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(120), stream.next())
            .await
            .expect("first turn completes")
            .expect("stream remains open")
            .expect("valid live event");
        match event {
            AgentEvent::SessionStarted { session_id: id, .. } => session_id = Some(id),
            AgentEvent::TextDelta { text } => first_turn.push_str(&text),
            AgentEvent::Done { status, .. } => {
                assert_eq!(status, DoneStatus::Completed);
                break;
            }
            _ => {}
        }
    }
    assert!(session_id.is_some(), "live session id missing");
    assert!(first_turn.contains("done"), "first turn text: {first_turn}");

    // Parked followup: the persistent child serves the next turn immediately.
    steer_tx
        .send(SteerMessage {
            prompt: "Reply with exactly: again".into(),
            message_id: None,
        })
        .await
        .unwrap();
    let mut saw_boundary = false;
    let mut second_turn = String::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(120), stream.next())
            .await
            .expect("second turn completes")
            .expect("stream remains open for the followup")
            .expect("valid live event");
        match event {
            AgentEvent::Steered { .. } => saw_boundary = true,
            AgentEvent::TextDelta { text } => second_turn.push_str(&text),
            AgentEvent::Done { status, .. } => {
                assert_eq!(status, DoneStatus::Completed);
                break;
            }
            _ => {}
        }
    }
    assert!(
        saw_boundary,
        "parked followup must cross a steering boundary"
    );
    assert!(
        second_turn.contains("again"),
        "second turn text: {second_turn}"
    );

    drop(steer_tx);
    assert!(
        tokio::time::timeout(Duration::from_secs(15), stream.next())
            .await
            .expect("mailbox closure reaps the live RPC child")
            .is_none()
    );
}
