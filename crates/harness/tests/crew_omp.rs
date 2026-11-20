//! Crew: OMP's driver against a stand-in `omp` (fixtures/crew-fake-omp.sh).
//! Ported from Crew's tests/omp.rs where its cases still apply; the journal
//! writer check, takeover and fork are not part of this port.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt as _;
use futures::stream::BoxStream;
use tokio::sync::{mpsc, oneshot};
use zeron_harness::crew_inference::{ManagedRoute, Provider};
use zeron_harness::{CancellationToken, Harness, HarnessError, OmpHarness, RunControls, SteerMessage, TurnControl};
use zeron_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, ToolCall, UserInputAnswer};

type Events = BoxStream<'static, Result<AgentEvent, HarnessError>>;

fn fake() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/crew-fake-omp.sh")
}

/// A run's own directory: its cwd, its agent directory (sessions) and logs.
struct Rig {
    dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn harness(&self) -> OmpHarness {
        OmpHarness::new()
            .with_executable(fake())
            .with_agent_dir(self.path("agent"))
            .with_graces(Duration::from_millis(100), Duration::from_millis(500))
    }

    fn request(&self, prompt: &str) -> RunRequest {
        let log = |name: &str| self.path(name).display().to_string();
        RunRequest {
            prompt: prompt.into(),
            harness: None,
            model: Some("anthropic/claude-opus-5-5".into()),
            reasoning: None,
            model_options: Default::default(),
            cwd: self.dir.path().display().to_string(),
            sandbox: SandboxLevel::DangerFullAccess,
            auto_approve: true,
            access: Default::default(),
            plan: false,
            resume: None,
            attachments: Vec::new(),
            worktree: None,
            mcp: None,
            extra_mcp: Vec::new(),
            env: BTreeMap::from([
                ("OMP_ARGV_LOG".to_owned(), log("argv")),
                ("OMP_ENV_LOG".to_owned(), log("env")),
                ("OMP_RPC_PID_LOG".to_owned(), log("pid")),
                ("OMP_PROMPT_LOG".to_owned(), log("prompts")),
            ]),
        }
    }

    /// A journal in OMP's sessions, its header naming `id`.
    fn journal(&self, id: &str) -> PathBuf {
        let dir = self.path("agent/sessions/--project--");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("2026-11-20_{id}.jsonl"));
        std::fs::write(&path, format!("{}\n", serde_json::json!({"type": "session", "id": id, "cwd": "/tmp"}))).unwrap();
        path
    }

    fn argv(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("argv")).unwrap().lines().map(str::to_owned).collect()
    }

    fn pid(&self) -> i32 {
        std::fs::read_to_string(self.path("pid")).unwrap().trim().parse().unwrap()
    }
}

struct Controls {
    steer: mpsc::Sender<SteerMessage>,
    interrupt: CancellationToken,
    turn: TurnControl,
}

/// The engine's side: answers every question (Continue, else the first
/// choice), a steering mailbox, an interrupt and the turn's controls.
fn engine() -> (RunControls, Controls) {
    let (steer, steering) = mpsc::channel(8);
    let interrupt = CancellationToken::new();
    let turn = TurnControl::default();
    let run = RunControls {
        realtime: None,
        execution_lease: None,
        request_input: Box::new(|questions| {
            let answers = questions
                .iter()
                .map(|question| UserInputAnswer {
                    question_id: question.id.clone(),
                    labels: vec![if question.options.iter().any(|option| option == "Continue") {
                        "Continue".into()
                    } else {
                        question.options.first().cloned().unwrap_or_else(|| "typed".into())
                    }],
                })
                .collect();
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(answers);
            rx
        }),
        steering,
        interrupt: interrupt.clone(),
        turn: turn.clone(),
    };
    (run, Controls { steer, interrupt, turn })
}

/// Every event up to the stream's end.
async fn to_end(stream: Events) -> Vec<Result<AgentEvent, HarnessError>> {
    tokio::time::timeout(Duration::from_secs(20), stream.collect()).await.expect("the run ends")
}

/// Events up to and including the next `Done`.
async fn to_done(stream: &mut Events) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), stream.next())
            .await
            .expect("a Done comes")
            .expect("the stream stays open")
            .expect("a valid event");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

fn ok(events: Vec<Result<AgentEvent, HarnessError>>) -> Vec<AgentEvent> {
    events.into_iter().map(|event| event.expect("a valid event")).collect()
}

fn text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn dones(events: &[AgentEvent]) -> Vec<(DoneStatus, Option<String>)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Done { status, error, .. } => Some((*status, error.clone())),
            _ => None,
        })
        .collect()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks whether the fixture's process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test]
async fn a_turn_streams_text_tools_and_usage_and_ends_on_agent_end() {
    let rig = Rig::new();
    let (run, controls) = engine();
    drop(controls.steer);
    let events = ok(to_end(rig.harness().run(rig.request("hello"), run).await.unwrap()).await);
    let AgentEvent::SessionStarted { harness, model, session_id, .. } = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!((*harness, model.as_str(), session_id.as_str()), (HarnessId::Omp, "anthropic/claude-opus-5-5", "omp-session-1"));
    assert_eq!(text(&events), "hello from omp");
    assert!(events.contains(&AgentEvent::ReasoningDelta { text: "thinking".into() }));
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::ToolCall { id, call: ToolCall::ReadFile { path } } if id == "tool-1" && path == "README.md")));
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::ToolResult { id, output: Some(output), is_error: false, .. } if id == "tool-1" && output == "# README")));
    assert!(events.contains(&AgentEvent::Usage { input_tokens: 106, output_tokens: 4 }));
    assert!(events.contains(&AgentEvent::ContextUsage { tokens: Some(110), window: Some(1_000_000) }));
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::AvailableCommands { commands } if commands.iter().any(|command| command.name == "ralplan"))));
    assert!(matches!(events.last(), Some(AgentEvent::Done { status: DoneStatus::Completed, error: None, session_id: Some(id), .. }) if id == "omp-session-1"));
    let argv = rig.argv();
    assert_eq!(&argv[..4], ["--mode", "rpc", "--approval-mode", "yolo"]);
    let flag = |name: &str| argv.windows(2).find(|pair| pair[0] == name).map(|pair| pair[1].clone());
    assert_eq!(flag("--model").as_deref(), Some("anthropic/claude-opus-5-5"));
    assert_eq!(flag("--thinking").as_deref(), Some("medium"));
    assert!(flag("--extension").is_none(), "an unmanaged run loads no gateway");
    // The run's overlay went with it.
    let overlay = argv.windows(2).filter(|pair| pair[0] == "--config").last().unwrap()[1].clone();
    assert!(!Path::new(&overlay).exists(), "{overlay} outlived its run");
    let prompts = std::fs::read_to_string(rig.path("prompts")).unwrap();
    assert!(prompts.contains(r#""streamingBehavior":"steer""#), "{prompts}");
}

/// A local command answers without the agent: its output, then Done.
#[tokio::test]
async fn a_local_command_completes_without_a_model_turn() {
    let rig = Rig::new();
    let (run, controls) = engine();
    drop(controls.steer);
    let events = ok(to_end(rig.harness().run(rig.request("/mcp"), run).await.unwrap()).await);
    assert!(text(&events).starts_with("    MCP server management\n    \n    /mcp list"), "{events:?}");
    assert_eq!(dones(&events), [(DoneStatus::Completed, None)]);
}

/// A turn OMP began itself (Crew's `TurnStarted`): the engine is told it
/// woke once it has taken the last turn's Done, before the new turn's first
/// words, and the turn ends with its own Done.
#[tokio::test]
async fn an_autonomous_turn_wakes_the_engine_before_its_output() {
    let rig = Rig::new();
    let (run, controls) = engine();
    let mut stream = rig.harness().run(rig.request("scenario:autonomous"), run).await.unwrap();
    // OMP began its own turn at once, but the engine hasn't read the last
    // turn's Done: a wake now would be spent inside that turn.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(10), controls.turn.woke()).await.is_err(),
        "the wake came before the engine had the last turn's Done"
    );
    let first = to_done(&mut stream).await;
    tokio::time::timeout(Duration::from_secs(2), controls.turn.woke()).await.expect("the wake was noted");
    let second = to_done(&mut stream).await;
    drop(controls.steer);
    assert_eq!(dones(&first), [(DoneStatus::Completed, None)]);
    assert_eq!(dones(&second), [(DoneStatus::Completed, None)]);
    assert!(matches!(second.first(), Some(AgentEvent::SessionStarted { .. })), "{second:?}");
    assert_eq!(text(&second), "autonomous reply");
    assert!(!first.iter().chain(&second).any(|event| matches!(event, AgentEvent::Steered { .. })), "no steer was sent");
    assert!(to_end(stream).await.is_empty());
}

/// Crew's `state_refresh_drains_burst_events_before_the_response`: a goal
/// command's state read keeps draining OMP's frames while it waits.
#[tokio::test]
async fn a_goal_refresh_drains_burst_events_before_its_answer() {
    let rig = Rig::new();
    let (run, controls) = engine();
    drop(controls.steer);
    let mut request = rig.request("/goal set Persistent editor indicator");
    request.env.insert("OMP_STATE_BURST".into(), "1".into());
    request.env.insert("OMP_OMIT_GOAL_EVENTS".into(), "1".into());
    let events = ok(to_end(rig.harness().run(request, run).await.unwrap()).await);
    assert_eq!(text(&events), "queued event\n".repeat(512));
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::ToolCall { call: ToolCall::Unknown { name, input: Some(input) }, .. }
            if name == "OMP goal" && input["goal"]["objective"] == "Persistent editor indicator")));
    assert_eq!(dones(&events), [(DoneStatus::Completed, None)]);
}

/// The todo tool's state becomes upstream's live plan.
#[tokio::test]
async fn todos_become_the_live_plan() {
    let rig = Rig::new();
    let (run, controls) = engine();
    drop(controls.steer);
    let events = ok(to_end(rig.harness().run(rig.request("scenario:todo"), run).await.unwrap()).await);
    let plan = events.iter().find_map(|event| match event {
        AgentEvent::ToolCall { id, call: ToolCall::Todo { items } } if id == zeron_proto::LIVE_PLAN_TOOL_ID => Some(items),
        _ => None,
    });
    let items: Vec<_> = plan.expect("a plan").iter().map(|item| (item.text.as_str(), item.done)).collect();
    assert_eq!(items, [("read", true), ("fix", false)]);
}

/// Crew's `resume_path_and_id_use_the_same_canonical_handshake`.
#[tokio::test]
async fn a_resume_by_id_or_path_opens_the_exact_journal() {
    let rig = Rig::new();
    let journal = rig.journal("resume-session");
    let canonical = std::fs::canonicalize(&journal).unwrap();
    for selector in ["resume-session", journal.to_str().unwrap()] {
        let (run, controls) = engine();
        drop(controls.steer);
        let mut request = rig.request("hello");
        request.resume = Some(selector.into());
        let events = ok(to_end(rig.harness().run(request, run).await.unwrap()).await);
        assert!(events.iter().any(|event| matches!(event,
            AgentEvent::SessionStarted { session_id, .. } if session_id == "resume-session")), "{events:?}");
        let argv = rig.argv();
        let resumed = argv.windows(2).find(|pair| pair[0] == "--resume").map(|pair| PathBuf::from(&pair[1]));
        assert_eq!(resumed.as_ref(), Some(&canonical), "{selector}");
    }
}

/// As upstream's other agents: a session that can't be reopened starts
/// afresh and says so, rather than strand the chat on it.
#[tokio::test]
async fn an_unknown_session_starts_afresh_and_says_so() {
    let rig = Rig::new();
    let (run, controls) = engine();
    drop(controls.steer);
    let mut request = rig.request("hello");
    request.resume = Some("gone-session".into());
    let events = ok(to_end(rig.harness().run(request, run).await.unwrap()).await);
    assert!(matches!(&events[0], AgentEvent::SessionStarted { session_id, .. } if session_id == "omp-session-1"));
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::Error { message } if message.contains("could not restore session gone-session"))), "{events:?}");
    assert!(!rig.argv().iter().any(|arg| arg == "--resume"));
    assert_eq!(dones(&events), [(DoneStatus::Completed, None)]);
}

/// Crew's `resume_path_still_rejects_a_different_native_session`.
#[tokio::test]
async fn a_resume_that_opens_another_session_fails() {
    let rig = Rig::new();
    let journal = rig.journal("expected-session");
    let (run, controls) = engine();
    drop(controls.steer);
    let mut request = rig.request("hello");
    request.resume = Some(journal.display().to_string());
    request.env.insert("OMP_REPORTED_SESSION_ID".into(), "wrong-session".into());
    let events = to_end(rig.harness().run(request, run).await.unwrap()).await;
    assert!(events.iter().any(|event| matches!(event, Err(error) if error.to_string().contains("instead of expected-session"))), "{events:?}");
    assert!(!events.iter().any(|event| matches!(event, Ok(AgentEvent::SessionStarted { .. }))));
}

/// Crew's `native_retry_continuation_finishes_without_an_error_card`: a
/// retry is the same turn, and its recovered error shows nowhere.
#[tokio::test]
async fn a_retry_continuation_finishes_without_an_error_card() {
    let rig = Rig::new();
    let (run, controls) = engine();
    drop(controls.steer);
    let events = ok(to_end(rig.harness().run(rig.request("scenario:retry"), run).await.unwrap()).await);
    let mut parts = Vec::new();
    for event in &events {
        zeron_doc::fold_event_into_parts(&mut parts, event);
    }
    assert_eq!(dones(&events), [(DoneStatus::Completed, None)]);
    assert!(parts.iter().any(|part| matches!(part, zeron_doc::MessagePart::Text { text, .. } if text == "hello from omp")));
    assert!(!parts.iter().any(|part| matches!(part, zeron_doc::MessagePart::Error { .. })), "{parts:?}");
    let before: Vec<_> = parts
        .iter()
        .filter_map(|part| match part {
            zeron_doc::MessagePart::Tool { id, resolved, is_error, .. } if id == "before-retry" => Some((*resolved, *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(before, [(true, false)]);
}

/// Crew's `errored_rpc_run_releases_writer_before_done`: OMP is reaped
/// before the errored Done, which carries the one visible error; the session
/// then resumes.
#[tokio::test]
async fn an_errored_turn_reaps_omp_before_its_done() {
    let rig = Rig::new();
    rig.journal("errored-session");
    let (run, controls) = engine();
    let mut request = rig.request("hello");
    request.resume = Some("errored-session".into());
    request.env.insert("OMP_TURN_ERROR".into(), "1".into());
    let mut stream = rig.harness().run(request, run).await.unwrap();
    let events = to_done(&mut stream).await;
    assert!(!alive(rig.pid()), "the errored Done came while OMP still ran");
    let error = "No API key for provider: comet-anthropic";
    assert_eq!(dones(&events), [(DoneStatus::Errored, Some(error.into()))]);
    let mut parts = Vec::new();
    for event in &events {
        zeron_doc::fold_event_into_parts(&mut parts, event);
    }
    let errors: Vec<_> = parts
        .iter()
        .filter_map(|part| match part {
            zeron_doc::MessagePart::Error { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(errors, [error]);
    assert!(to_end(stream).await.is_empty(), "the run ended with its turn");
    drop(controls.steer);

    let (run, controls) = engine();
    drop(controls.steer);
    let mut request = rig.request("hello");
    request.resume = Some("errored-session".into());
    let resumed = ok(to_end(rig.harness().run(request, run).await.unwrap()).await);
    assert_eq!(dones(&resumed), [(DoneStatus::Completed, None)]);
}

/// Crew's `hung_abort_is_force_killed_before_the_session_can_resume`, less
/// the writer check: an abort OMP never answers has its grace, then OMP is
/// ended before the interrupted Done.
#[tokio::test]
async fn a_hung_abort_is_force_killed_and_the_session_resumes() {
    let rig = Rig::new();
    rig.journal("hung-session");
    let (run, controls) = engine();
    let mut request = rig.request("hello");
    request.resume = Some("hung-session".into());
    request.env.insert("OMP_STEER_SCENARIO".into(), "1".into());
    request.env.insert("OMP_HANG_ABORT".into(), "1".into());
    let mut stream = rig.harness().run(request, run).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next()).await.unwrap().unwrap().unwrap();
        if matches!(event, AgentEvent::TextDelta { .. }) {
            break;
        }
    }
    controls.interrupt.cancel();
    let events = ok(tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("a hung abort is ended within the interrupt's deadline"));
    assert!(matches!(events.last(), Some(AgentEvent::Done { status: DoneStatus::Interrupted, .. })), "{events:?}");
    assert!(!alive(rig.pid()), "OMP outlived the interrupt");

    let (run, controls) = engine();
    drop(controls.steer);
    let mut request = rig.request("hello");
    request.resume = Some("hung-session".into());
    let resumed = ok(to_end(rig.harness().run(request, run).await.unwrap()).await);
    assert_eq!(dones(&resumed), [(DoneStatus::Completed, None)]);
}

/// Crew's `turn_stop_keeps_native_process_but_stream_drop_reaps_before_releasing_lease`.
#[tokio::test]
async fn a_stopped_turn_keeps_omp_and_a_dropped_stream_reaps_it_before_the_lease() {
    let rig = Rig::new();
    let gate = std::sync::Arc::new(tokio::sync::RwLock::new(()));
    let lease = std::sync::Arc::new(gate.clone().read_owned().await);
    let (mut run, controls) = engine();
    run.execution_lease = Some(lease);
    let mut request = rig.request("hello");
    request.env.insert("OMP_STEER_SCENARIO".into(), "1".into());
    let mut stream = rig.harness().run(request, run).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next()).await.unwrap().unwrap().unwrap();
        if matches!(event, AgentEvent::TextDelta { .. }) {
            break;
        }
    }
    let pid = rig.pid();
    controls.turn.stop_turn();
    let events = to_done(&mut stream).await;
    assert_eq!(dones(&events), [(DoneStatus::Interrupted, None)]);
    assert!(alive(pid), "a turn's stop must not end the session");
    assert!(gate.clone().try_write_owned().is_err(), "the run holds its lease");
    drop(stream);
    let _exclusive = tokio::time::timeout(Duration::from_secs(10), gate.write()).await.unwrap();
    assert!(!alive(pid), "the lease went before OMP did");
}

/// A stop with no turn open is spent there: the next turn, from a steer to
/// the parked session, runs to its end.
#[tokio::test]
async fn a_stop_with_no_turn_open_is_not_carried_into_the_next() {
    let rig = Rig::new();
    let (run, controls) = engine();
    let mut stream = rig.harness().run(rig.request("hello"), run).await.unwrap();
    assert_eq!(dones(&to_done(&mut stream).await), [(DoneStatus::Completed, None)]);
    controls.turn.stop_turn();
    controls.steer.send(SteerMessage::text("again")).await.unwrap();
    let next = to_done(&mut stream).await;
    assert!(matches!(next.first(), Some(AgentEvent::Steered { next_assistant_message_id: Some(_), .. })), "{next:?}");
    assert_eq!(dones(&next), [(DoneStatus::Completed, None)]);
    assert_eq!(text(&next), "hello from omp");
    drop(controls.steer);
    assert!(to_end(stream).await.is_empty());
    assert_eq!(std::fs::read_to_string(rig.path("prompts")).unwrap().lines().count(), 2);
}

/// A steer during the live turn joins it: one Done for both.
#[tokio::test]
async fn a_steer_joins_the_live_turn() {
    let rig = Rig::new();
    let (run, controls) = engine();
    let mut request = rig.request("hello");
    request.env.insert("OMP_STEER_SCENARIO".into(), "1".into());
    let mut stream = rig.harness().run(request, run).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next()).await.unwrap().unwrap().unwrap();
        if matches!(event, AgentEvent::TextDelta { .. }) {
            break;
        }
    }
    controls.steer.send(SteerMessage::text("also this")).await.unwrap();
    let events = to_done(&mut stream).await;
    assert!(matches!(events.first(), Some(AgentEvent::Steered { .. })), "{events:?}");
    assert_eq!(text(&events), "steered reply");
    assert_eq!(dones(&events), [(DoneStatus::Completed, None)]);
}

/// An extension's dialogs reach the person and their answers OMP.
#[tokio::test]
async fn dialogs_are_answered_in_omp_s_terms() {
    let rig = Rig::new();
    let (run, controls) = engine();
    drop(controls.steer);
    let events = ok(to_end(rig.harness().run(rig.request("scenario:elicitation"), run).await.unwrap()).await);
    assert_eq!(dones(&events), [(DoneStatus::Completed, None)], "{events:?}");
}

/// A managed run reaches OMP with the gateway's variables and extension, its
/// model and roles on the route, and never the relay token's own name.
#[tokio::test]
async fn a_managed_run_reaches_omp_through_the_gateway_only() {
    let rig = Rig::new();
    let (run, controls) = engine();
    drop(controls.steer);
    let mut request = rig.request("hello");
    // As the engine's `prepare` leaves it: the route's model, unprefixed.
    request.model = Some("claude-opus-5-5".into());
    request.env.extend(
        ManagedRoute {
            provider: Provider::Anthropic,
            base_url: "http://127.0.0.1:9".into(),
            token: "opaque-local-token".into(),
        }
        .env(),
    );
    let events = ok(to_end(rig.harness().run(request, run).await.unwrap()).await);
    assert!(matches!(&events[0], AgentEvent::SessionStarted { model, .. } if model == "anthropic/claude-opus-5-5"), "{events:?}");
    assert_eq!(dones(&events), [(DoneStatus::Completed, None)]);
    let argv = rig.argv();
    let flag = |name: &str| argv.windows(2).find(|pair| pair[0] == name).map(|pair| pair[1].clone());
    assert_eq!(flag("--model").as_deref(), Some("comet-anthropic/claude-opus-5-5"));
    let extension = flag("--extension").expect("the gateway");
    assert!(extension.ends_with("agent-auth.ts"));
    assert!(!Path::new(&extension).exists(), "the gateway outlived its run");
    let env = std::fs::read_to_string(rig.path("env")).unwrap();
    let var = |name: &str| env.lines().find_map(|line| line.strip_prefix(&format!("{name}=")).map(str::to_owned));
    assert_eq!(var("OMP_AUTH_GATEWAY_URL").as_deref(), Some("http://127.0.0.1:9"));
    assert_eq!(var("OMP_AUTH_GATEWAY_TOKEN").as_deref(), Some("opaque-local-token"));
    assert_eq!(var("PI_SMOL_MODEL").as_deref(), Some("comet-anthropic/claude-opus-5-5"));
    assert_eq!(var("COMET_INFERENCE_TOKEN"), None, "a user's crew-auth-gateway.ts activates on it");
    assert_eq!(var("PI_CODING_AGENT_DIR").map(PathBuf::from), Some(rig.path("agent")));
}

/// The real `omp`'s model list as a managed engine probes it: offline, in a
/// scratch home, with nothing of this machine's environment but the probe's
/// placeholder keys. Run it with `CREW_OMP_EXECUTABLE=$(command -v omp)`.
#[tokio::test]
#[ignore = "runs the real omp's offline model list: set CREW_OMP_EXECUTABLE"]
async fn the_real_omp_lists_its_models_as_a_managed_engine_probes_them() {
    use std::os::unix::fs::PermissionsExt as _;
    let omp = PathBuf::from(std::env::var_os("CREW_OMP_EXECUTABLE").expect("CREW_OMP_EXECUTABLE names an omp"));
    let rig = Rig::new();
    let home = rig.path("home");
    std::fs::create_dir(&home).unwrap();
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
    let wrapper = rig.path("omp");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec /usr/bin/env -i HOME={} PATH=\"$PATH\" ANTHROPIC_API_KEY=\"$ANTHROPIC_API_KEY\" \
             ANTHROPIC_BASE_URL=\"$ANTHROPIC_BASE_URL\" OPENAI_API_KEY=\"$OPENAI_API_KEY\" \
             OPENAI_BASE_URL=\"$OPENAI_BASE_URL\" {} \"$@\"\n",
            quote(&home),
            quote(&omp)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    zeron_harness::crew_inference::set_managed_discovery(true);
    let models = OmpHarness::new().with_executable(&wrapper).with_agent_dir(rig.path("agent")).models().await.unwrap();
    let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
    for wanted in ["anthropic/claude-opus-5-5", "openai/gpt-6.2-sol"] {
        assert!(ids.contains(&wanted), "{wanted}: {ids:?}");
    }
    assert!(
        ids.iter().all(|id| Provider::for_model(HarnessId::Omp, id).is_some()),
        "a managed probe lists only Agent Auth's providers: {ids:?}"
    );
    let opus = models.iter().find(|model| model.id == "anthropic/claude-opus-5-5").unwrap();
    assert!(!opus.reasoning_levels.is_empty() && !opus.default);
    eprintln!("omp listed {} models", models.len());
}

/// Crew's `catalog_uses_native_omp_model_and_command_discovery`.
#[tokio::test]
async fn models_and_commands_come_from_omp() {
    let rig = Rig::new();
    let harness = rig.harness();
    let models = harness.models().await.unwrap();
    let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["anthropic/claude-opus-5-5", "openai/gpt-6.2-sol"]);
    assert_eq!(models[1].label, "GPT-6.2 Sol");
    let commands = harness.commands_for(rig.dir.path()).await.unwrap();
    for name in ["ralplan", "advisor", "goal"] {
        assert!(commands.iter().any(|command| command.name == name), "{name}: {commands:?}");
    }
    let guard = commands.iter().find(|command| command.name == "guard").expect("the guard's path").description.clone();
    assert!(guard.ends_with("guard.js"), "{guard}");
    assert!(!Path::new(&guard).exists(), "the guard outlived discovery");
}
