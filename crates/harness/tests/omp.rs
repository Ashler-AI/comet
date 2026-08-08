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
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].name, "ralplan");
    assert_eq!(commands[0].input_hint.as_deref(), Some("goal"));
    assert_eq!(commands[1].name, "security");
}

#[tokio::test]
async fn fake_omp_proves_acp_only_execution_and_event_mapping() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
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
            .map(|event| event.expect("valid ACP event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes");

    let argv = std::fs::read_to_string(argv_log).unwrap();
    assert_eq!(argv.lines().next(), Some("acp"));
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
            } if model == "default"
                && tools.is_empty()
                && cwd.is_empty()
                && session_id == "omp-session-1" =>
            {
                Some(assistant_message_id)
            }
            _ => None,
        })
        .expect("OMP session start");
    assert!(assistant_message_id.starts_with("acp-omp-session-1-"));
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
async fn acp_provider_elicitation_round_trips_through_shared_input() {
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
            .map(|event| event.expect("valid ACP event"))
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
    assert_eq!(asked[0].header, "Permission");
    assert_eq!(asked[0].options, ["Allow once", "Reject"]);
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
async fn selected_model_and_reasoning_are_applied_before_prompting() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let config_log = temp.path().join("config");
    unsafe {
        std::env::set_var("OMP_CONFIG_LOG", &config_log);
        std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv"));
    }
    let harness = OmpHarness::new().with_executable(fixture_path());
    let mut request = request(None);
    request.model = Some("openai-codex/gpt-5.6-sol".into());
    request.reasoning = Some(ReasoningLevel::XHigh);
    let stream = harness.run(request, controls()).await.expect("run starts");
    tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid ACP event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes");

    let requests: Vec<serde_json::Value> = std::fs::read_to_string(config_log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["method"], "session/set_config_option");
    assert_eq!(requests[0]["params"]["configId"], "model");
    assert_eq!(requests[0]["params"]["value"], "openai-codex/gpt-5.6-sol");
    assert_eq!(requests[1]["params"]["configId"], "thinking");
    assert_eq!(requests[1]["params"]["value"], "xhigh");
    unsafe {
        std::env::remove_var("OMP_CONFIG_LOG");
    }
}

#[tokio::test]
async fn resume_uses_the_requested_acp_session_id() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let method_log = temp.path().join("method");
    let argv_log = temp.path().join("argv");
    let session_dir = temp.path().join("sessions");
    write_omp_session(&session_dir, "resume-session");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
        std::env::set_var("OMP_METHOD_LOG", &method_log);
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
    assert_eq!(
        std::fs::read_to_string(method_log).unwrap().trim(),
        "session/load",
        "OMP's advertised stable load path must be used for a materialized handoff"
    );
    unsafe {
        std::env::remove_var("OMP_METHOD_LOG");
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
async fn persistent_run_queues_steer_while_acp_prompt_is_active() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let prompt_log = temp.path().join("prompts");
    let session_log = temp.path().join("sessions");
    let prompt_gate = temp.path().join("prompt-gate");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv"));
        std::env::set_var("OMP_PROMPT_LOG", &prompt_log);
        std::env::set_var("OMP_SESSION_LOG", &session_log);
        std::env::set_var("OMP_PROMPT_GATE", &prompt_gate);
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
    assert!(prompt_log.is_file(), "first ACP prompt did not start");
    steer_tx
        .send(SteerMessage {
            prompt: "follow up".into(),
            message_id: Some("user-follow-up".into()),
        })
        .await
        .unwrap();
    std::fs::write(&prompt_gate, b"release").unwrap();
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

    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("second turn completes")
            .expect("stream remains open for second turn")
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
    assert!(
        first_done < boundary && boundary < second_done,
        "events: {events:?}"
    );
    assert_eq!(
        std::fs::read_to_string(session_log).unwrap(),
        "session/new\n",
        "one ACP session must serve both turns"
    );
    let prompts = std::fs::read_to_string(prompt_log).unwrap();
    assert_eq!(prompts.lines().count(), 2);
    assert!(prompts.lines().nth(1).unwrap().contains("follow up"));
    unsafe {
        std::env::remove_var("OMP_PROMPT_LOG");
        std::env::remove_var("OMP_SESSION_LOG");
        std::env::remove_var("OMP_PROMPT_GATE");
    }
}

#[tokio::test]
async fn acp_error_details_reach_the_harness_error() {
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
            .expect("ACP error arrives")
        {
            Some(Err(error)) => break error,
            Some(Ok(_)) => {}
            None => panic!("stream closed without the ACP error"),
        }
    };
    let message = error.to_string();
    assert!(message.contains("openai-codex/gpt-5.6-sol"));
    assert!(message.contains("billing limit reached"));
    assert!(!message.ends_with("Internal error"));
    unsafe { std::env::remove_var("OMP_PROMPT_ERROR_DETAILS") };
}
