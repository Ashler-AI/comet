#![cfg(unix)]

use comet_harness::{AcpHarness, CancellationToken, Harness, RunControls, SteerMessage};
use comet_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel};
use futures::{StreamExt, stream::BoxStream};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

type Events = BoxStream<'static, Result<AgentEvent, comet_harness::HarnessError>>;

fn fixture(agent: &str) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join(format!("{agent}.py"));
    std::os::unix::fs::symlink(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-acp.py"),
        &executable,
    )
    .unwrap();
    (directory, executable)
}

fn request(prompt: &str, resume: Option<&str>) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("chosen".into()),
        agent_account_id: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: resume.map(str::to_string),
        attachments: Vec::new(),
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (sender, receiver) = mpsc::channel(4);
    let interrupt = CancellationToken::new();
    (
        RunControls {
            request_input: Box::new(|_| {
                let (sender, receiver) = oneshot::channel();
                let _ = sender.send(Vec::new());
                receiver
            }),
            steering: receiver,
            interrupt: interrupt.clone(),
            context: None,
        },
        sender,
        interrupt,
    )
}

async fn turn(events: &mut Events) -> (String, DoneStatus) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut text = String::new();
        loop {
            match events
                .next()
                .await
                .expect("stream ended before Done")
                .expect("ACP event")
            {
                AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                AgentEvent::Done { status, .. } => return (text, status),
                _ => {}
            }
        }
    })
    .await
    .expect("turn settled")
}

#[tokio::test]
async fn acp_agents_discover_select_stream_queue_and_resume() {
    for (name, factory) in [
        ("devin", AcpHarness::devin as fn() -> AcpHarness),
        ("grok", AcpHarness::grok),
        ("hermes", AcpHarness::hermes),
        ("pi", AcpHarness::pi),
    ] {
        let (_directory, executable) = fixture(name);
        let harness = factory().with_executable(executable);
        let models = harness.models().await.expect("live model catalog");
        assert_eq!(models[0].id, "chosen");
        let commands = harness.commands("").await.expect("live command catalog");
        assert_eq!(commands[0].name, "review");
        let (controls, sender, interrupt) = controls();
        let mut events = harness.run(request("first", None), controls).await.unwrap();
        sender
            .send(SteerMessage {
                prompt: "second".into(),
                message_id: Some("second-message".into()),
            })
            .await
            .unwrap();
        assert_eq!(
            turn(&mut events).await,
            ("answer:first".into(), DoneStatus::Completed),
            "{name}"
        );
        assert_eq!(
            turn(&mut events).await,
            ("answer:second".into(), DoneStatus::Completed),
            "{name}"
        );
        interrupt.cancel();
        drop(events);
        let (controls, _sender, interrupt) = self::controls();
        let mut resumed = harness
            .run(request("resumed", Some("acp-session")), controls)
            .await
            .unwrap();
        assert_eq!(
            turn(&mut resumed).await,
            ("answer:resumed".into(), DoneStatus::Completed),
            "{name}"
        );
        interrupt.cancel();
    }
}

#[tokio::test]
async fn acp_permission_gate_fails_closed_even_with_auto_approve() {
    let (_directory, executable) = fixture("hermes");
    let harness = AcpHarness::hermes().with_executable(executable);
    let (mut controls, _sender, interrupt) = controls();
    let (asked, mut questions) = mpsc::unbounded_channel();
    controls.request_input = Box::new(move |input| {
        let (reply, receiver) = oneshot::channel();
        asked.send((input, reply)).unwrap();
        receiver
    });
    let mut events = harness
        .run(request("permission", None), controls)
        .await
        .unwrap();
    let (input, reply) = tokio::time::timeout(Duration::from_secs(5), questions.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(input[0].question.contains("Sensitive action"));
    assert_eq!(input[0].options, ["Allow once", "Deny"]);
    drop(reply); // No interactive approval: cancellation, never auto-allow.
    assert_eq!(
        turn(&mut events).await,
        ("denied".into(), DoneStatus::Completed)
    );
    interrupt.cancel();
}

#[tokio::test]
async fn acp_cancel_interrupts_a_live_prompt_and_model_mismatch_is_visible() {
    let (_directory, executable) = fixture("hermes");
    let harness = AcpHarness::hermes().with_executable(executable);
    let (controls, _sender, interrupt) = controls();
    let mut events = harness.run(request("wait", None), controls).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.next().await {
            if matches!(event.unwrap(), AgentEvent::TextDelta { ref text } if text == "waiting") {
                break;
            }
        }
    })
    .await
    .unwrap();
    interrupt.cancel();
    assert_eq!(turn(&mut events).await.1, DoneStatus::Interrupted);

    let (controls, _sender, _) = self::controls();
    let mut invalid = request("must not run", None);
    invalid.model = Some("unadvertised".into());
    let mut events = harness.run(invalid, controls).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not advertise requested model")
    );
}

#[tokio::test]
async fn acp_rejects_shared_authority_and_hidden_model_overrides() {
    let (_directory, executable) = fixture("hermes");
    let harness = AcpHarness::hermes().with_executable(executable);
    let mut routed = request("must not run", None);
    routed.agent_account_id = Some("shared-account".into());
    let (controls, _, _) = self::controls();
    match harness.run(routed, controls).await {
        Err(error) => assert!(error.to_string().contains("own CLI authentication")),
        Ok(_) => panic!("ACP accepted an unsupported shared account"),
    }
    let (controls, _, _) = self::controls();
    let mut invalid = request("must not run", None);
    invalid
        .model_options
        .insert("selected_model".into(), serde_json::json!("different"));
    let mut events = harness.run(invalid, controls).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("cannot override"));
}
