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
        resume: resume.map(str::to_owned),
        attachments: Vec::new(),
    }
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
    unsafe { std::env::remove_var("OMP_ARGV_LOG"); }
}
