use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard};

use comet_harness::{
    CancellationToken, Harness, InferenceRoute, PrimeAgentHarness, RunContext, RunControls,
    SteerMessage,
};
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

fn argument_after<'a>(argv: &'a str, flag: &str) -> &'a str {
    argv.lines()
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|pair| (pair[0] == flag).then_some(pair[1]))
        .unwrap_or_else(|| panic!("{flag} has no adjacent value in {argv:?}"))
}

#[cfg(unix)]
fn assert_private_unix_socket(socket: &Path) {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    assert!(socket.is_absolute());
    assert_eq!(
        socket.file_name().and_then(|name| name.to_str()),
        Some("p.sock")
    );
    assert!(
        socket
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("comet-pa-"))
    );
    assert!(socket.as_os_str().as_bytes().len() <= 103);
    assert_eq!(
        std::fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

fn routed_controls(token: &str) -> RunControls {
    let mut controls = controls();
    controls.context = Some(RunContext {
        session_id: "comet-session".into(),
        ipc_port: 38117,
        inference: Some(InferenceRoute {
            base_url: "http://127.0.0.1:41234".into(),
            token: token.into(),
            provider: "openai".into(),
            model: "gpt-5.6-sol".into(),
        }),
        fork_from: None,
    });
    controls
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
    let harness = PrimeAgentHarness::new().with_executable(fixture_path());
    let config_dir = temp.path().join("config");
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", temp.path().join("argv"));
    let _coding_agent_dir = EnvGuard::set("PRIME_AGENT_CODING_AGENT_DIR", &config_dir);
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
    let session_dir = PathBuf::from(argument_after(&argv, "--session-dir"));
    let socket_path = PathBuf::from(argument_after(&argv, "--daemon-socket"));
    assert!(session_dir.is_absolute());
    assert_eq!(
        session_dir.parent(),
        Some(config_dir.join("comet-sessions").as_path())
    );
    #[cfg(unix)]
    assert_private_unix_socket(&socket_path);
    assert_eq!(argument_after(&argv, "--session-store"), "prime");
    assert_eq!(argument_after(&argv, "--model"), "openai-codex/gpt-5.6-sol");
    assert_eq!(argument_after(&argv, "--thinking"), "high");
    assert_eq!(argument_after(&argv, "--mode"), "acp");
    assert!(
        !argv
            .lines()
            .any(|argument| argument == "--dangerously-skip-permissions")
    );
    #[cfg(unix)]
    {
        let runtime_dir = socket_path.parent().unwrap().to_path_buf();
        let listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("prepared Comet runtime directory accepts the Prime socket");
        drop(listener);
        std::fs::remove_file(socket_path).unwrap();
        std::fs::remove_dir(runtime_dir).unwrap();
    }
}

#[tokio::test]
async fn prime_resume_uses_the_native_token_without_creating_a_parallel_store() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let resume_log = temp.path().join("resume");
    let config_dir = temp.path().join("config");
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", temp.path().join("argv"));
    let _resume_log = EnvGuard::set("PRIME_RESUME_LOG", &resume_log);
    let _coding_agent_dir = EnvGuard::set("PRIME_AGENT_CODING_AGENT_DIR", &config_dir);
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
    let socket_path = PathBuf::from(argument_after(&argv, "--daemon-socket"));
    #[cfg(unix)]
    assert_private_unix_socket(&socket_path);
    assert_eq!(argument_after(&argv, "--resume"), "native-prime-session");
    assert_eq!(argument_after(&argv, "--mode"), "acp");
    assert!(!argv.lines().any(|arg| arg == "--session-dir"));
    assert!(config_dir.join("comet-runtime").is_dir());
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SessionStarted { session_id, .. } if session_id == "native-prime-session"
    )));
    #[cfg(unix)]
    std::fs::remove_dir(socket_path.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn prime_fork_uses_native_fork_without_reusing_the_source_identity() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join("config");
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", temp.path().join("argv"));
    let _coding_agent_dir = EnvGuard::set("PRIME_AGENT_CODING_AGENT_DIR", &config_dir);
    let harness = PrimeAgentHarness::new().with_executable(fixture_path());
    let mut fork_controls = controls();
    fork_controls.context = Some(RunContext {
        session_id: "crew-fork".into(),
        ipc_port: 38117,
        inference: None,
        fork_from: Some("native-prime-session".into()),
    });

    let events = harness
        .run(request(Some("native-prime-session")), fork_controls)
        .await
        .expect("fork starts")
        .map(|event| event.expect("valid ACP event"))
        .collect::<Vec<_>>()
        .await;

    let argv = std::fs::read_to_string(temp.path().join("argv")).unwrap();
    assert_eq!(argument_after(&argv, "--fork"), "native-prime-session");
    assert!(!argv.lines().any(|arg| arg == "--resume"));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SessionStarted { session_id, .. } if session_id == "prime-acp-session"
    )));
}

#[tokio::test]
async fn routed_run_resolves_one_absolute_agent_dir_without_persisting_credentials() {
    let _env = env_lock().await;
    let current_dir = std::env::current_dir().unwrap();
    let agent_root = tempfile::tempdir_in(&current_dir).unwrap();
    let child_cwd = tempfile::tempdir().unwrap();
    let relative_agent_dir = Path::new(agent_root.path().file_name().unwrap());
    let argv_log = child_cwd.path().join("argv");
    let scoped_token = "scoped-loopback-token";
    let opaque_account_id = "opaque-account-id";
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", &argv_log);
    let _coding_agent_dir = EnvGuard::set("PRIME_AGENT_CODING_AGENT_DIR", relative_agent_dir);
    let harness = PrimeAgentHarness::new().with_executable(fixture_path());
    let mut routed_request = request(None);
    routed_request.cwd = child_cwd.path().to_string_lossy().into_owned();
    routed_request.agent_account_id = Some(opaque_account_id.into());
    let stream = harness
        .run(routed_request, routed_controls(scoped_token))
        .await
        .expect("routed run starts");
    tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid ACP event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes");

    let agent_dir = agent_root.path();
    let argv = std::fs::read_to_string(argv_log).unwrap();
    let extension = PathBuf::from(argument_after(&argv, "--extension"));
    let session_dir = PathBuf::from(argument_after(&argv, "--session-dir"));
    let socket = PathBuf::from(argument_after(&argv, "--daemon-socket"));
    assert!(extension.is_absolute());
    assert!(session_dir.is_absolute());
    assert!(socket.is_absolute());
    assert_eq!(
        extension,
        agent_dir.join("comet-runtime/agent-auth-gateway.ts")
    );
    assert_eq!(
        session_dir.parent(),
        Some(agent_dir.join("comet-sessions").as_path())
    );
    #[cfg(unix)]
    {
        assert_private_unix_socket(&socket);
        assert!(!socket.starts_with(agent_dir));
    }
    assert_eq!(argument_after(&argv, "--provider"), "comet-openai");
    assert_eq!(argument_after(&argv, "--model"), "gpt-5.6-sol");
    assert!(!argv.contains(scoped_token));
    assert!(!argv.contains(opaque_account_id));
    assert!(!argv.lines().any(|argument| argument == "--api-key"));
    let extension_source = std::fs::read_to_string(extension).unwrap();
    assert!(!extension_source.contains(scoped_token));
    assert!(!extension_source.contains(opaque_account_id));
    #[cfg(unix)]
    std::fs::remove_dir(socket.parent().unwrap()).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn long_configured_dir_uses_a_short_bindable_unix_endpoint() {
    use std::os::unix::ffi::OsStrExt as _;

    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let long_component = "configured-prime-agent-".repeat(8);
    let config_dir = temp.path().join(&long_component).join(&long_component);
    assert!(config_dir.as_os_str().as_bytes().len() > 300);
    let argv_log = temp.path().join("argv");
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", &argv_log);
    let _coding_agent_dir = EnvGuard::set("PRIME_AGENT_CODING_AGENT_DIR", &config_dir);
    let harness = PrimeAgentHarness::new().with_executable(fixture_path());
    let stream = harness
        .run(request(None), controls())
        .await
        .expect("long configured directory run starts");
    tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid ACP event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes");

    let argv = std::fs::read_to_string(argv_log).unwrap();
    let socket = PathBuf::from(argument_after(&argv, "--daemon-socket"));
    assert_private_unix_socket(&socket);
    assert!(socket.as_os_str().as_bytes().len() < config_dir.as_os_str().as_bytes().len());
    let runtime_dir = socket.parent().unwrap().to_path_buf();
    let listener = std::os::unix::net::UnixListener::bind(&socket)
        .expect("short hashed endpoint binds despite a long configured agent directory");
    drop(listener);
    std::fs::remove_file(socket).unwrap();
    std::fs::remove_dir(runtime_dir).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_a_precreated_symlink_at_the_unix_endpoint_parent() {
    let _env = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join("config");
    let argv_log = temp.path().join("argv");
    let _argv_log = EnvGuard::set("PRIME_ARGV_LOG", &argv_log);
    let _coding_agent_dir = EnvGuard::set("PRIME_AGENT_CODING_AGENT_DIR", &config_dir);
    let harness = PrimeAgentHarness::new().with_executable(fixture_path());
    let stream = harness
        .run(request(None), controls())
        .await
        .expect("initial run prepares the endpoint parent");
    tokio::time::timeout(
        Duration::from_secs(10),
        stream
            .map(|event| event.expect("valid ACP event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("fake completes");

    let argv = std::fs::read_to_string(&argv_log).unwrap();
    let socket = PathBuf::from(argument_after(&argv, "--daemon-socket"));
    let endpoint_parent = socket.parent().unwrap().to_path_buf();
    std::fs::remove_dir(&endpoint_parent).unwrap();
    let symlink_target = temp.path().join("attacker-controlled");
    std::fs::create_dir(&symlink_target).unwrap();
    std::os::unix::fs::symlink(&symlink_target, &endpoint_parent).unwrap();

    let error = match harness.run(request(None), controls()).await {
        Ok(_) => panic!("symlink endpoint parent must be rejected before spawn"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("symbolic link"));
    assert!(!symlink_target.join("p.sock").exists());
    assert!(
        std::fs::symlink_metadata(&endpoint_parent)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    std::fs::remove_file(endpoint_parent).unwrap();
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
