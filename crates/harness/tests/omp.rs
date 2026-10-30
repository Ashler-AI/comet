#![cfg(unix)]
use std::path::PathBuf;
use tokio::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

async fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}
use std::time::Duration;

use zeron_harness::{CancellationToken, Harness, OmpHarness, RunControls};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel};
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
        realtime: None,
        execution_lease: None,
        turn: Default::default(),
    }
}

fn request(resume: Option<&str>) -> RunRequest {
    RunRequest {
        env: Default::default(),
        prompt: "hello".into(),
        model: None,
        harness: None,
        worktree: None,
        mcp: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::DangerFullAccess,
        auto_approve: true,
        access: Default::default(),
        plan: false,
        resume: resume.map(str::to_owned),
        attachments: Vec::new(),
    }
}
#[tokio::test]
async fn native_autonomous_turn_announces_start_before_output() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv")); }
    let (_sender, steering) = mpsc::channel(4);
    let mut controls = controls();
    controls.steering = steering;
    let mut request = request(None);
    request.prompt = "scenario:autonomous".into();
    let mut stream = OmpHarness::new().with_executable(fixture_path()).run(request, controls).await.unwrap();
    let mut starts = 0;
    let mut completed = 0;
    tokio::time::timeout(Duration::from_secs(10), async {
        while completed < 2 {
            match stream.next().await.expect("native runtime remains open").unwrap() {
                AgentEvent::TurnStarted => starts += 1,
                AgentEvent::TextDelta { .. } => assert_eq!(starts, completed + 1),
                AgentEvent::Done { status, .. } => {
                    assert_eq!(status, DoneStatus::Completed);
                    completed += 1;
                    assert_eq!(starts, completed);
                }
                AgentEvent::Steered { .. } => panic!("autonomous work must not acknowledge a user steer"),
                _ => {}
            }
        }
    }).await.unwrap();
    assert_eq!(starts, 2);
}

#[tokio::test]
async fn state_refresh_drains_burst_events_before_the_response() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv"));
        std::env::set_var("OMP_STATE_BURST", "1");
        std::env::set_var("OMP_OMIT_GOAL_EVENTS", "1");
    }
    let mut run_request = request(None);
    run_request.prompt = "/goal set Persistent editor indicator".into();
    let harness = OmpHarness::new().with_executable(fixture_path());
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = harness.run(run_request, controls()).await.unwrap();
        let mut text = String::new();
        let mut refreshed = false;
        let mut completed = false;
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                AgentEvent::ToolCall {
                    call: zeron_proto::ToolCall::Unknown { input: Some(input), .. }, ..
                } => {
                    refreshed = input["goal"]["objective"] == "Persistent editor indicator";
                }
                AgentEvent::Done { status: DoneStatus::Completed, .. } => completed = true,
                _ => {}
            }
        }
        (text, refreshed, completed)
    }).await;
    unsafe {
        std::env::remove_var("OMP_STATE_BURST");
        std::env::remove_var("OMP_OMIT_GOAL_EVENTS");
        std::env::remove_var("OMP_ARGV_LOG");
    }
    let (text, refreshed, completed) = result.expect("state refresh must drain the full event queue");
    assert_eq!(text, "queued event\n".repeat(512));
    assert!(refreshed, "goal state response was lost");
    assert!(completed, "turn completion was lost");
}

#[tokio::test]
async fn resume_path_and_id_use_the_same_canonical_handshake() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv");
    let session_dir = temp.path().join("sessions");
    let journal = write_omp_session(&session_dir, "resume-session");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", &argv_log);
        std::env::set_var("OMP_WRITER_STATE", "inactive");
    }
    let harness = OmpHarness::new()
        .with_executable(fixture_path())
        .with_session_dir(session_dir)
        .with_session_writer_probe(fixture_path());
    for selector in ["resume-session", journal.to_str().unwrap()] {
        let stream = harness
            .run(request(Some(selector)), controls())
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
    }
    unsafe {
        std::env::remove_var("OMP_WRITER_STATE");
    }
}

#[tokio::test]
async fn native_retry_continuation_finishes_without_an_error_card() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let session_dir = temp.path().join("sessions");
    write_omp_session(&session_dir, "retry-session");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv"));
        std::env::set_var("OMP_WRITER_STATE", "inactive");
    }
    let harness = OmpHarness::new()
        .with_executable(fixture_path())
        .with_session_dir(session_dir)
        .with_session_writer_probe(fixture_path());
    let mut request = request(Some("retry-session"));
    request.prompt = "scenario:retry".into();
    let mut stream = harness.run(request, controls()).await.unwrap();
    let mut parts = Vec::new();
    let mut starts = 0;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("native retry settles")
            .expect("stream remains open through recovery")
            .expect("valid recovery event");
        if matches!(event, AgentEvent::TurnStarted) { starts += 1; }
        zeron_doc::fold_event_into_parts(&mut parts, &event);
        if let AgentEvent::Done { status, error, session_id, .. } = event {
            assert_eq!(status, DoneStatus::Completed);
            assert_eq!(error, None);
            assert_eq!(session_id.as_deref(), Some("retry-session"));
            break;
        }
    }
    assert_eq!(starts, 2, "retry starts must not clear completed tool output");
    assert!(parts.iter().any(|part| matches!(part,
        zeron_doc::MessagePart::Text { text, .. } if text == "hello from omp"
    )));
    assert!(!parts.iter().any(|part| matches!(part, zeron_doc::MessagePart::Error { .. })));
    let completed: Vec<_> = parts.iter().filter_map(|part| match part {
        zeron_doc::MessagePart::Tool { id, resolved, is_error, .. } if id == "before-retry" => {
            Some((*resolved, *is_error))
        }
        _ => None,
    }).collect();
    assert_eq!(completed, [(true, false)]);
    unsafe {
        std::env::remove_var("OMP_WRITER_STATE");
    }
}

#[tokio::test]
async fn inactive_takeover_is_idempotent_and_invalid_or_ambiguous_journals_fail() {
    let temp = tempfile::tempdir().unwrap();
    let journal = write_omp_session(temp.path(), "inactive-session");
    let harness = OmpHarness::new()
        .with_executable("/missing/omp")
        .with_session_dir(temp.path());
    harness
        .stop_session(journal.to_str().unwrap())
        .await
        .unwrap();
    harness.stop_session("inactive-session").await.unwrap();
    let unrelated_writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap();
    assert!(harness.stop_session("inactive-session").await.is_err());
    assert!(
        unrelated_writer.metadata().is_ok(),
        "unrelated holder must remain untouched"
    );
    drop(unrelated_writer);
    harness.stop_session("inactive-session").await.unwrap();
    let invalid = temp.path().join("invalid.jsonl");
    std::fs::write(&invalid, "{}\n").unwrap();
    assert!(
        harness
            .stop_session(invalid.to_str().unwrap())
            .await
            .is_err()
    );
    std::fs::copy(&journal, temp.path().join("duplicate.jsonl")).unwrap();
    assert!(harness.stop_session("inactive-session").await.is_err());
}

#[tokio::test]
async fn resume_path_still_rejects_a_different_native_session() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let journal = write_omp_session(temp.path(), "expected-session");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv"));
        std::env::set_var("OMP_WRITER_STATE", "inactive");
        std::env::set_var("OMP_REPORTED_SESSION_ID", "wrong-session");
    }
    let stream = OmpHarness::new()
        .with_executable(fixture_path())
        .with_session_dir(temp.path())
        .with_session_writer_probe(fixture_path())
        .run(request(Some(journal.to_str().unwrap())), controls())
        .await
        .unwrap();
    let events = tokio::time::timeout(Duration::from_secs(10), stream.collect::<Vec<_>>())
        .await
        .unwrap();
    unsafe {
        std::env::remove_var("OMP_REPORTED_SESSION_ID");
        std::env::remove_var("OMP_WRITER_STATE");
    }
    assert!(events.iter().any(Result::is_err));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(AgentEvent::SessionStarted { .. })))
    );
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
        error.to_string().contains("already has a writer"),
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
        realtime: None,
        execution_lease: None,
        turn: Default::default(),
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
    assert!(error.to_string().contains("already has a writer"));

    interrupt.cancel();
    let events = tokio::time::timeout(
        Duration::from_secs(5),
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
    let mut parts = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("errored turn settles")
            .expect("stream remains open")
            .expect("valid errored event");
        zeron_doc::fold_event_into_parts(&mut parts, &event);
        match event {
            AgentEvent::Done { status, error, .. } => {
                assert_eq!(status, DoneStatus::Errored);
                assert_eq!(error.as_deref(), Some("No API key for provider: comet-anthropic"));
                let errors: Vec<_> = parts
                    .iter()
                    .filter_map(|part| match part {
                        zeron_doc::MessagePart::Error { message, .. } => Some(message.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(errors, ["No API key for provider: comet-anthropic"]);
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
        zeron_harness::omp::session_writer_state(&journal),
        zeron_harness::omp::SessionWriterState::Active
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
        zeron_harness::omp::session_writer_state(&journal),
        zeron_harness::omp::SessionWriterState::Inactive
    );
}

#[tokio::test]
async fn turn_stop_keeps_native_process_but_stream_drop_reaps_before_releasing_lease() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let pid_path = temp.path().join("pid");
    unsafe {
        std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv"));
        std::env::set_var("OMP_RPC_PID_LOG", &pid_path);
        std::env::set_var("OMP_STEER_SCENARIO", "1");
    }
    let gate = std::sync::Arc::new(tokio::sync::RwLock::new(()));
    let lease = std::sync::Arc::new(gate.clone().read_owned().await);
    let (_sender, steering) = mpsc::channel(4);
    let turn = zeron_harness::TurnControl::default();
    let mut controls = controls();
    controls.steering = steering;
    controls.turn = turn.clone();
    controls.execution_lease = Some(lease);
    let mut stream = OmpHarness::new().with_executable(fixture_path())
        .run(request(None), controls).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = stream.next().await {
            if matches!(event.unwrap(), AgentEvent::TextDelta { .. }) { break; }
        }
    }).await.unwrap();
    let pid: i32 = std::fs::read_to_string(&pid_path).unwrap().trim().parse().unwrap();
    turn.stop_turn();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let AgentEvent::Done { status, .. } = stream.next().await.unwrap().unwrap() {
                assert_eq!(status, DoneStatus::Interrupted);
                break;
            }
        }
    }).await.unwrap();
    assert!(process_exists(pid), "turn stop must not tear down native runtime");
    assert!(gate.clone().try_write_owned().is_err(), "runtime still owns execution lease");
    drop(stream);
    let _exclusive = tokio::time::timeout(Duration::from_secs(10), gate.write()).await.unwrap();
    assert!(!process_exists(pid), "lease released before child was reaped");
    unsafe {
        std::env::remove_var("OMP_ARGV_LOG");
        std::env::remove_var("OMP_RPC_PID_LOG");
        std::env::remove_var("OMP_STEER_SCENARIO");
    }
}

#[tokio::test]
async fn catalog_uses_native_omp_model_and_command_discovery() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("OMP_ARGV_LOG", temp.path().join("argv")); }
    let harness = OmpHarness::new().with_executable(fixture_path());
    let models = harness.models().await.unwrap();
    assert!(models.iter().any(|model| model.id == "openai-codex/gpt-5.6-sol"));
    let commands = harness.commands_for(temp.path()).await.unwrap();
    assert!(commands.iter().any(|command| command.name == "ralplan"));
    assert!(commands.iter().any(|command| command.name == "advisor"));
    let argv = std::fs::read_to_string(temp.path().join("argv")).unwrap();
    let args: Vec<_> = argv.lines().collect();
    let guard = args.windows(2).find(|pair| pair[0] == "--extension").unwrap()[1];
    assert!(!std::path::Path::new(guard).exists(), "catalog guard must be removed after reaping");
    unsafe { std::env::remove_var("OMP_ARGV_LOG"); }
}

#[tokio::test]
#[ignore = "requires OMP_EXECUTABLE; isolated native OMP with loopback Responses SSE only"]
async fn real_omp_recovers_idle_stream_without_replaying_completed_tool() {
    use std::os::unix::fs::PermissionsExt as _;

    let _env = env_lock().await;
    let executable = std::env::var_os("OMP_EXECUTABLE").expect("set OMP_EXECUTABLE to the native OMP binary");
    let executable = std::fs::canonicalize(executable).unwrap();
    let temp = tempfile::tempdir().unwrap();
    // OMP deliberately leaves HOME on startup; keep it separate from the workspace.
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let agent = temp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(temp.path().join("completed.txt"), "completed before the stall\n").unwrap();
    let extension = temp.path().join("retry-provider.ts");
    std::fs::write(&extension, include_str!("fixtures/omp-retry-provider.ts")).unwrap();
    let config = temp.path().join("settings.yml");
    std::fs::write(&config, "retry:\n  maxRetries: 2\n  baseDelayMs: 1\n  modelFallback: false\nfeatures:\n  unexpectedStopDetection: none\n").unwrap();
    let quote = |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let wrapper = temp.path().join("omp-probe");
    std::fs::write(&wrapper, format!(
        "#!/bin/sh\nexec /usr/bin/env -i HOME={} PATH={} ASHLER_INCREMENTAL_TSC_CHECKS=false PI_CODING_AGENT_DIR={} PI_OPENAI_STREAM_IDLE_TIMEOUT_MS=250 PI_OPENAI_STATEFUL=0 {} \"$@\" --extension {}\n",
        quote(&home), quote(std::path::Path::new(&std::env::var_os("PATH").unwrap_or_default())),
        quote(&agent), quote(&executable), quote(&extension),
    )).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let harness = OmpHarness::new().with_executable(wrapper).with_session_dir(agent.join("sessions"));
    let mut request = request(None);
    request.cwd = temp.path().display().to_string();
    request.model = Some("crew-retry-fixture/fixture".into());
    request.prompt = "Read completed.txt, then reply RECOVERED.".into();
    // Isolate inherited overlays too; Crew's generated run config still comes last.
    // OmpRunConfig::apply converts these paths to repeated --config arguments;
    // the env -i wrapper preserves them through "$@", not PI_CONFIG_FILES.
    let inherited = std::env::var_os("PI_CONFIG_FILES");
    unsafe { std::env::set_var("PI_CONFIG_FILES", &config); }
    let run = harness.run(request, controls()).await;
    unsafe {
        match inherited {
            Some(value) => std::env::set_var("PI_CONFIG_FILES", value),
            None => std::env::remove_var("PI_CONFIG_FILES"),
        }
    }
    let events: Vec<AgentEvent> = tokio::time::timeout(Duration::from_secs(45), run.unwrap().collect::<Vec<_>>())
        .await.expect("native retry must settle")
        .into_iter().collect::<Result<_, _>>().unwrap();
    let mut parts = Vec::new();
    for event in &events {
        zeron_doc::fold_event_into_parts(&mut parts, event);
    }
    assert!(matches!(events.last(), Some(AgentEvent::Done {
        status: DoneStatus::Completed, error: None, ..
    })), "native events: {events:?}");
    assert!(!parts.iter().any(|part| matches!(part, zeron_doc::MessagePart::Error { .. })));
    assert!(parts.iter().any(|part| matches!(part,
        zeron_doc::MessagePart::Text { text, .. } if text == "RECOVERED"
    )));
    let reads = events.iter().filter(|event| matches!(event,
        AgentEvent::ToolCall { call: zeron_proto::ToolCall::ReadFile { .. }, .. }
    )).count();
    assert_eq!(reads, 1, "completed read must not execute again");
    assert!(parts.iter().any(|part| matches!(part,
        zeron_doc::MessagePart::Tool { resolved: true, is_error: false, .. }
    )), "native parts: {parts:?}; events: {events:?}");
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::ToolResult { is_error: false, output: Some(output), .. }
            if output.contains("completed before the stall")
    )), "native events: {events:?}");
    let requests: Vec<serde_json::Value> = std::fs::read_to_string(temp.path().join("retry-requests.jsonl"))
        .unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(requests.len(), 3, "tool turn, stalled attempt, recovered attempt");
    let results: Vec<_> = requests[2]["input"].as_array().unwrap().iter()
        .filter(|item| item["type"] == "function_call_output").collect();
    assert_eq!(results.len(), 1, "recovered request retains the completed tool result once");
    assert!(results[0]["output"].to_string().contains("completed before the stall"));
}
