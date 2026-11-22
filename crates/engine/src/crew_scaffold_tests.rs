//! Crew main's mocked-Platform tests (`scaffold.rs`), adapted: a fake
//! Platform and a fake Edge on one loopback origin, scripted per request.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use zeron_harness::CancellationToken;
use zeron_proto::HarnessId;
use zeron_rpc::{StaticToken, TokenSource};

use super::*;

pub(crate) const SESSION: &str = "011664b5-3660-4fe6-83a2-3647fa6a2f65";
pub(crate) const SANDBOX: &str = "sandbox-a";
pub(crate) const DEVICE: &str = "comet-scaffold-sandbox-a-e1";
pub(crate) const GRANT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const EDGE: &str = "https://crew-zgx-edge.example";

pub(crate) fn scope() -> ScaffoldScope {
    ScaffoldScope {
        project_id: "project-a".into(),
        deployment_id: Some("deployment-a".into()),
        session_id: Some(SESSION.into()),
    }
}

/// A sandbox as Platform answers it, in `status`.
pub(crate) fn sandbox_value(status: &str) -> Value {
    json!({
        "id": SANDBOX, "lifecycleEpoch": 1, "status": status, "kind": "remote_code",
        "runtimeProfile": "comet_remote", "region": "harness-zgx",
        "cometRuntimeProfile": {"version": "scaffold.comet-runtime.v1", "projectId": "project-a",
            "deploymentId": "deployment-a", "sessionId": SESSION, "sandboxId": SANDBOX, "lifecycleEpoch": 1,
            "targetDeviceId": DEVICE, "actor": {"sub": "alice@example.com"}},
        "agentRoute": {"provider": "anthropic", "model": "claude-opus-5-5", "fallback": "disabled", "routingMode": "automatic"},
        "sourceRef": "387d6652abd642f0b85e8bd14f9131a9f23b7e70", "ownerEmail": "alice@example.com",
        "createdAt": "2026-11-20T00:00:00Z", "updatedAt": "2026-11-20T00:01:00Z", "lastActivityAt": "2026-11-20T00:02:00Z",
        "links": {"terminal": "https://terminal.example"},
    })
}

pub(crate) fn sandbox(status: &str) -> String {
    json!({"ok": true, "sandbox": sandbox_value(status)}).to_string()
}

/// The host's probe answer: its grant as one line, Crew's.
pub(crate) fn host_authority(grant_id: &str, expires_at: i64) -> String {
    json!({
        "ok": true, "exitCode": 0,
        "stdout": format!("{}\n", json!({
            "grantId": grant_id, "expiresAt": expires_at, "principalSubject": "alice@example.com",
            "scope": scope(), "sandboxId": SANDBOX, "deviceId": DEVICE, "lifecycleEpoch": 1,
            "capabilities": host_capabilities(),
        })),
    })
    .to_string()
}

fn grant_answer(expires_at: i64) -> String {
    json!({"grant": format!("cg1.{GRANT}.narrow_secret"), "expiresAt": expires_at,
        "accessExpiresAt": expires_at + 12 * 3_600_000})
    .to_string()
}

pub(crate) enum Mock {
    Http { status: u16, body: String },
    /// Headers that promise a body, then nothing.
    HeadersThenClose,
    CancelThenClose(CancellationToken),
}

pub(crate) fn ok(body: impl Into<String>) -> Mock {
    Mock::Http { status: 200, body: body.into() }
}

pub(crate) fn status(status: u16, body: impl Into<String>) -> Mock {
    Mock::Http { status, body: body.into() }
}

/// A loopback origin answering each request with what `respond` says; it
/// stops after the answer `respond` marks last, and hands back every
/// request (line, headers and body) in order.
pub(crate) async fn mock_server(
    mut respond: impl FnMut(&str, &Value) -> (Mock, bool) + Send + 'static,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let mut finished = false;
        while !finished {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0_u8; 8192];
                let read = stream.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    let length = String::from_utf8_lossy(&bytes[..end])
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            if bytes.is_empty() {
                continue;
            }
            let request = String::from_utf8(bytes).unwrap();
            let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
            let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let (response, last) = respond(&request, &value);
            finished = last;
            requests.push(request);
            match response {
                Mock::HeadersThenClose => {
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 10\r\nconnection: close\r\n\r\n")
                        .await;
                }
                Mock::CancelThenClose(cancellation) => cancellation.cancel(),
                Mock::Http { status, body } => {
                    let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 {status} Answer\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                }
            }
        }
        requests
    });
    (origin, task)
}

/// Answers in order; the last one ends the server.
pub(crate) async fn mock_steps(steps: Vec<Mock>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let mut steps = VecDeque::from(steps);
    mock_server(move |_, _| {
        let step = steps.pop_front().expect("an answer for every request");
        (step, steps.is_empty())
    })
    .await
}

fn bearer(token: &str) -> Arc<dyn TokenSource> {
    Arc::new(StaticToken(token.into()))
}

fn hosts(origin: &str, token: &str) -> SandboxHosts {
    let client = ScaffoldClient::new(origin, "project-a", bearer(token)).unwrap();
    let grants = Arc::new(EdgeDeviceJoinGrantClient::new(origin, bearer(token)).unwrap());
    SandboxHosts::new(client, EDGE, grants).unwrap()
}

fn line(request: &str) -> &str {
    request.lines().next().unwrap()
}

fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

fn execs(requests: &[String]) -> Vec<Value> {
    requests
        .iter()
        .filter(|request| request.starts_with(&format!("POST /api/code-sandboxes/{SANDBOX}/exec ")))
        .map(|request| body(request))
        .collect()
}

/// List, inspect and create: the bearer on each, the v1 authority profile
/// and the route on the create, the region named; a list keeps only this
/// project's sandboxes that pass the checks, and a v2 profile never does.
#[tokio::test]
async fn the_control_plane_wire_is_crew_s() {
    let mut other_session = sandbox_value("ready");
    other_session["id"] = json!("sandbox-b");
    other_session["cometRuntimeProfile"]["sandboxId"] = json!("sandbox-b");
    other_session["cometRuntimeProfile"]["sessionId"] = json!("9dd173da-fb53-48df-9581-026009c783e2");
    let mut other_project = sandbox_value("ready");
    other_project["cometRuntimeProfile"]["projectId"] = json!("project-b");
    let mut no_owner = sandbox_value("ready");
    no_owner["ownerEmail"] = Value::Null;
    let listed = json!({"ok": true, "sandboxes": [sandbox_value("paused"), other_session, other_project, no_owner]}).to_string();
    let (origin, captured) = mock_steps(vec![
        ok(listed),
        ok(sandbox("ready")),
        status(202, sandbox("starting")),
        ok(sandbox("ready").replace("scaffold.comet-runtime.v1", "scaffold.comet-runtime.v2")),
    ])
    .await;
    let client = ScaffoldClient::new(&origin, "project-a", bearer("sc_rc_control_secret")).unwrap();
    let cancellation = CancellationToken::new();
    let mut all = scope();
    all.session_id = None;
    let listed = client.list(&all, &cancellation).await.unwrap();
    assert_eq!(listed.len(), 2, "this project's two sandboxes; the one with no owner is left out");
    assert_eq!(listed[0].sandbox().unwrap(), (SANDBOX, Lifecycle::Paused, Some(1)));
    assert_eq!(listed[0].region(), Some("harness-zgx"));
    assert_eq!(listed[0].scope, scope());
    assert_eq!(listed[1].scope.session_id.as_deref(), Some("9dd173da-fb53-48df-9581-026009c783e2"));
    client.inspect(SANDBOX, &scope(), &cancellation).await.unwrap();
    let route = AgentRoute::for_chat(&HarnessId::Omp, "anthropic/claude-opus-5-5", Some("account-1")).unwrap();
    let options = CreateOptions { region: Some("harness-zgx"), ..Default::default() };
    let created = client.create(&scope(), options, &route, &cancellation).await.unwrap();
    assert_eq!(created.sandbox().unwrap(), (SANDBOX, Lifecycle::Starting, Some(1)));
    assert_eq!(created.owner_principal, "alice@example.com");
    assert!(matches!(
        client.inspect(SANDBOX, &scope(), &cancellation).await,
        Err(ScaffoldError::InvalidResponse(_))
    ));

    let requests = captured.await.unwrap();
    let lines: Vec<_> = requests.iter().map(|request| line(request)).collect();
    assert_eq!(
        lines,
        [
            "GET /api/code-sandboxes HTTP/1.1",
            "GET /api/code-sandboxes/sandbox-a HTTP/1.1",
            "POST /api/code-sandboxes HTTP/1.1",
            "GET /api/code-sandboxes/sandbox-a HTTP/1.1",
        ]
    );
    assert!(requests.iter().all(|request| request.to_ascii_lowercase().contains("authorization: bearer sc_rc_control_secret")));
    assert_eq!(
        body(&requests[2]),
        json!({
            "region": "harness-zgx",
            "databaseEnvironment": "local",
            "agentRoute": {"provider": "anthropic", "model": "claude-opus-5-5", "fallback": "disabled",
                "routingMode": "pinned", "accountId": "account-1"},
            "cometRuntimeProfile": {"version": "scaffold.comet-runtime.v1", "projectId": "project-a",
                "deploymentId": "deployment-a", "sessionId": SESSION},
        })
    );
}

/// A create is idempotent by session: Platform derives the sandbox from the
/// authority profile, so asking again for the same session and route
/// answers the same sandbox; another route for it is refused (409), a
/// rejection, never a second sandbox.
#[tokio::test]
async fn a_create_is_idempotent_by_session() {
    let created = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
    let seen = created.clone();
    let mut requests = 0;
    let (origin, captured) = mock_server(move |_, body| {
        requests += 1;
        let session = body["cometRuntimeProfile"]["sessionId"].as_str().unwrap().to_owned();
        let mut created = seen.lock().unwrap();
        let answer = match created.iter().find(|(id, _)| *id == session) {
            None => {
                created.push((session, body["agentRoute"].clone()));
                status(202, sandbox("starting"))
            }
            Some((_, route)) if *route == body["agentRoute"] => ok(sandbox("starting")),
            Some(_) => status(409, r#"{"error":"comet_runtime_lifecycle_conflict"}"#),
        };
        (answer, requests == 3)
    })
    .await;
    let client = ScaffoldClient::new(&origin, "project-a", bearer("sc_rc_owner")).unwrap();
    let cancellation = CancellationToken::new();
    let route = AgentRoute::for_chat(&HarnessId::Omp, "openai/gpt-6.2-sol", None).unwrap();
    let options = CreateOptions { region: Some("harness-zgx"), ..Default::default() };
    let first = client.create(&scope(), options, &route, &cancellation).await.unwrap();
    let again = client.create(&scope(), options, &route, &cancellation).await.unwrap();
    assert_eq!(first, again);
    let other = AgentRoute::for_chat(&HarnessId::Omp, "anthropic/claude-opus-5-5", None).unwrap();
    let refused = client.create(&scope(), options, &other, &cancellation).await.unwrap_err();
    assert_eq!(refused.api_error().map(|error| (error.status, error.code.as_str())), Some((409, "comet_runtime_lifecycle_conflict")));
    assert!(refused.is_rejection());
    assert_eq!(created.lock().unwrap().len(), 1, "one sandbox for one session");
    captured.await.unwrap();
}

/// 4xx is a rejection; the network, 5xx, a rate limit, a timeout or an
/// answer not understood leave the outcome unknown.
#[test]
fn only_a_4xx_is_a_rejection() {
    for code in [400, 401, 403, 404, 409, 422] {
        assert!(api_error(StatusCode::from_u16(code).unwrap(), b"{}", None).is_rejection(), "{code}");
    }
    for code in [408, 429, 500, 502, 503, 504] {
        assert!(!api_error(StatusCode::from_u16(code).unwrap(), b"{}", None).is_rejection(), "{code}");
    }
    assert!(!ScaffoldError::Cancelled.is_rejection());
    assert!(!ScaffoldError::InvalidResponse("garbled".into()).is_rejection());
    assert!(!ScaffoldError::AuthTemporarilyUnavailable.is_rejection());
    assert!(ScaffoldError::RuntimeContractMismatch.is_rejection());
    assert!(ScaffoldError::AttachRejected(Lifecycle::Failed).is_rejection());
    // Platform's provider wrapper promotes only its exact startup state.
    let starting = api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        br#"{"error":"sandbox_provider_error","body":{"error":"sandbox_runtime_starting"}}"#,
        None,
    );
    assert!(starting.is_runtime_starting());
    let message = api_error(StatusCode::BAD_REQUEST, br#"{"error":"remote_code_region_not_allowed","message":"Bearer sc_rc_secret"}"#, None);
    assert!(!message.to_string().contains("sc_rc_secret"), "{message}");
}

#[test]
fn only_https_or_loopback_origins() {
    for origin in ["https://scaffold-staging.internal.ashler.com", "http://127.0.0.1:8787", "https://edge.example/"] {
        assert!(ScaffoldClient::new(origin, "project-a", bearer("t")).is_ok(), "{origin}");
    }
    for origin in ["http://scaffold.example", "https://scaffold.example/api", "https://user@scaffold.example", "https://scaffold.example?x=1"] {
        assert!(ScaffoldClient::new(origin, "project-a", bearer("t")).is_err(), "{origin}");
    }
}

/// A chat's agent and model, as the sandbox's route: OMP's own ids (the
/// Cloud row's catalog), or Claude Code's and Codex's on their providers,
/// always hosted on OMP; an account pins it.
#[test]
fn a_chat_s_model_is_its_sandbox_s_route() {
    let route = AgentRoute::for_chat(&HarnessId::Omp, "anthropic/claude-opus-5-5", None).unwrap();
    assert_eq!((route.provider, route.model.as_str(), route.harness()), (AgentProvider::Anthropic, "claude-opus-5-5", HarnessId::Omp));
    assert_eq!(route.execution_model(), "anthropic/claude-opus-5-5");
    let codex = AgentRoute::for_chat(&HarnessId::Omp, "openai-codex/gpt-6.2-sol", None).unwrap();
    assert_eq!((codex.provider, codex.execution_model().as_str()), (AgentProvider::OpenAi, "openai/gpt-6.2-sol"));
    let claude = AgentRoute::for_chat(&HarnessId::ClaudeCode, "claude-opus-5-5", Some(" acct ")).unwrap();
    assert_eq!(
        serde_json::to_value(&claude).unwrap(),
        json!({"provider": "anthropic", "model": "claude-opus-5-5", "fallback": "disabled", "routingMode": "pinned", "accountId": "acct"})
    );
    assert_eq!(claude.harness(), HarnessId::Omp, "hosted on OMP");
    for (harness, model) in [
        (HarnessId::Omp, "claude-opus-5-5"),
        (HarnessId::Omp, "google/gemini"),
        (HarnessId::Omp, "anthropic/"),
        (HarnessId::ClaudeCode, "gpt-6.2-sol"),
        (HarnessId::Codex, "anthropic/claude"),
        (HarnessId::Cursor, "claude-opus-5-5"),
    ] {
        assert!(AgentRoute::for_chat(&harness, model, None).is_none(), "{harness:?} {model}");
    }
}

/// The probe confirms only exactly this sandbox's host: each field compared.
#[test]
fn the_probe_must_match_field_for_field() {
    let environment = serde_json::from_str::<SandboxEnvelope>(&sandbox("ready"))
        .unwrap()
        .sandbox
        .into_environment(&scope())
        .unwrap();
    let parse = |text: String| serde_json::from_str::<ExecResponse>(&text).unwrap();
    let future = now_ms() + 60_000;
    assert_eq!(confirmed(&parse(host_authority(GRANT, future)), &environment).unwrap().grant_id, GRANT);
    let exact: Value = serde_json::from_str(&host_authority(GRANT, future)).unwrap();
    let authority: Value = serde_json::from_str(exact["stdout"].as_str().unwrap()).unwrap();
    for (field, value) in [
        ("grantId", json!("")),
        ("expiresAt", json!(now_ms() - 1)),
        ("principalSubject", json!("mallory@example.com")),
        ("scope", json!({"projectId": "project-a", "deploymentId": "deployment-a", "sessionId": "other"})),
        ("sandboxId", json!("sandbox-b")),
        ("deviceId", json!("comet-scaffold-sandbox-a-e2")),
        ("lifecycleEpoch", json!(2)),
        ("capabilities", json!(["session.read"])),
        ("capabilities", json!(host_capabilities().into_iter().rev().collect::<Vec<_>>())),
    ] {
        let mut changed = authority.clone();
        changed[field] = value;
        let response = parse(json!({"ok": true, "exitCode": 0, "stdout": changed.to_string()}).to_string());
        assert!(confirmed(&response, &environment).is_none(), "{field}");
    }
    for response in [
        json!({"ok": false, "exitCode": 0, "stdout": authority.to_string()}),
        json!({"ok": true, "exitCode": 1, "stdout": authority.to_string()}),
        json!({"ok": true, "exitCode": 0, "stdout": "not json"}),
    ] {
        assert!(confirmed(&parse(response.to_string()), &environment).is_none(), "{response}");
    }
}

/// An exact host already serving: reused, with nothing minted, written or
/// started.
#[tokio::test]
async fn attach_reuses_an_exactly_bound_host() {
    let expires_at = now_ms() + 60_000;
    let (origin, captured) = mock_steps(vec![ok(sandbox("ready")), ok(host_authority(GRANT, expires_at))]).await;
    let (environment, attachment) = hosts(&origin, "sc_rc_broad_secret")
        .attach(SANDBOX, &scope(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(environment.sandbox().unwrap().0, SANDBOX);
    assert_eq!(
        attachment,
        Attachment {
            device_id: DEVICE.into(),
            run_id: None,
            grant: ControlGrant { id: GRANT.into(), expires_at, capabilities: host_capabilities() },
        }
    );
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        body(&requests[1]),
        json!({"argv": [".scaffold/crew/bin/comet", "--data-dir", ".scaffold/crew-data", "--ipc-port", "39400", "scaffold-authority"],
            "mode": "inline", "timeoutMs": 10_000})
    );
    assert!(!requests.iter().any(|request| request.contains("/auth/device-grants") || request.contains("--device-bootstrap-file")));
}

/// No host yet: a grant from this engine's Edge (Crew's seven capabilities,
/// in order), the bootstrap with Crew's seven fields as a private
/// workspace file, the native runner's 16-element launch in the
/// background, the probe's confirmation, and the file removed after. No
/// exec ever carries a credential.
#[tokio::test]
async fn attach_starts_the_host_as_the_native_runner_takes_it() {
    let expires_at = now_ms() + 60_000;
    let (origin, captured) = mock_steps(vec![
        ok(sandbox("starting")),
        ok(r#"{"ok":false,"exitCode":1}"#),
        ok(grant_answer(expires_at)),
        ok(r#"{"ok":true}"#),
        ok(r#"{"ok":true,"exitCode":0}"#),
        ok(r#"{"ok":true,"runId":"run-a","supervised":true}"#),
        ok(r#"{"ok":false,"exitCode":1,"stderr":"no engine yet"}"#),
        ok(host_authority(GRANT, expires_at + 12 * 3_600_000)),
        ok(r#"{"ok":true,"exitCode":0}"#),
    ])
    .await;
    let (environment, attachment) = hosts(&origin, "sc_rc_broad_secret")
        .attach(SANDBOX, &scope(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(environment.sandbox().unwrap().1, Lifecycle::Starting);
    assert_eq!((attachment.device_id.as_str(), attachment.run_id.as_deref()), (DEVICE, Some("run-a")));
    assert_eq!(attachment.grant.id, GRANT);
    assert!(!serde_json::to_string(&attachment).unwrap().contains("narrow_secret"));

    let requests = captured.await.unwrap();
    let grant = requests.iter().find(|request| request.starts_with("POST /auth/device-grants ")).unwrap();
    assert!(grant.to_ascii_lowercase().contains("authorization: bearer sc_rc_broad_secret"));
    assert_eq!(
        body(grant),
        json!({"targetDeviceId": DEVICE, "sessionId": SESSION, "deploymentId": "deployment-a", "sandboxId": SANDBOX,
            "lifecycleEpoch": 1, "capabilities": ["session.read", "session.sync.write", "session.chat", "session.control",
                "session.annotate", "session.files", "session.environment"], "ttlSeconds": 900})
    );
    let upload = requests.iter().find(|request| request.starts_with("PUT ")).unwrap();
    let target = upload.split_whitespace().nth(1).unwrap();
    let path = Url::parse(&format!("http://fixture{target}"))
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "path")
        .unwrap()
        .1
        .into_owned();
    assert!(target.starts_with(&format!("/api/code-sandboxes/{SANDBOX}/files?")));
    assert!(path.starts_with(".comet-device-bootstrap-") && path.ends_with(".json") && !path.contains('/'));
    let bootstrap: Value = serde_json::from_str(body(upload)["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        bootstrap,
        json!({"deviceJoinGrant": format!("cg1.{GRANT}.narrow_secret"), "projectId": "project-a", "deploymentId": "deployment-a",
            "sessionId": SESSION, "deviceId": DEVICE, "lifecycleEpoch": 1, "sandboxId": SANDBOX})
    );
    let execs = execs(&requests);
    assert_eq!(execs.len(), 6, "probe, chmod, launch, two probes, cleanup");
    assert_eq!(execs[1], json!({"argv": ["chmod", "600", path], "mode": "inline", "timeoutMs": 10_000}));
    assert_eq!(
        execs[2],
        json!({"argv": [".scaffold/crew/bin/comet", "--data-dir", ".scaffold/crew-data", "--ipc-port", "39400", "headless",
            "--device-bootstrap-file", path, "--edge-url", EDGE, "--scaffold-url", origin, "--project-scope", "project-a",
            "--deployment-id", "deployment-a"], "mode": "background", "timeoutMs": 0}),
        "the runner's exact argv: 16 elements, no env or cwd"
    );
    assert_eq!(execs[2]["argv"].as_array().unwrap().len(), 16);
    assert_eq!(execs[5]["argv"], json!(["rm", "-f", "--", path]));
    assert!(execs.iter().all(|exec| !exec.to_string().contains("cg1.") && !exec.to_string().contains("sc_rc_")));
}

/// While Platform says this epoch's runtime is starting, the probe waits
/// (inspecting the same lifecycle); a changed lifecycle ends the wait.
#[tokio::test]
async fn the_probe_waits_only_for_its_own_runtime_to_start() {
    let starting = r#"{"error":"sandbox_provider_error","body":{"error":"sandbox_runtime_starting"}}"#;
    let expires_at = now_ms() + 60_000;
    let (origin, captured) = mock_steps(vec![
        ok(sandbox("starting")),
        status(503, starting),
        ok(sandbox("starting")),
        ok(host_authority(GRANT, expires_at)),
    ])
    .await;
    let (_, attachment) = hosts(&origin, "t").attach(SANDBOX, &scope(), &CancellationToken::new()).await.unwrap();
    assert_eq!(attachment.grant.id, GRANT);
    assert_eq!(captured.await.unwrap().len(), 4);

    let mut epoch_two = sandbox_value("starting");
    epoch_two["lifecycleEpoch"] = json!(2);
    let (origin, captured) = mock_steps(vec![
        ok(sandbox("starting")),
        status(503, starting),
        ok(json!({"ok": true, "sandbox": epoch_two}).to_string()),
    ])
    .await;
    let error = hosts(&origin, "t").attach(SANDBOX, &scope(), &CancellationToken::new()).await.unwrap_err();
    assert!(matches!(error, ScaffoldError::InvalidResponse(_)), "{error}");
    assert!(!captured.await.unwrap().iter().any(|request| request.contains("/auth/device-grants")));
}

/// A stopped, failed or (for now) paused sandbox is refused before anything
/// changes.
#[tokio::test]
async fn a_terminal_or_paused_sandbox_is_refused() {
    for (state, lifecycle) in [("failed", Lifecycle::Failed), ("stopped", Lifecycle::Stopped), ("paused", Lifecycle::Paused)] {
        let (origin, captured) = mock_steps(vec![ok(sandbox(state))]).await;
        let error = hosts(&origin, "t").attach(SANDBOX, &scope(), &CancellationToken::new()).await.unwrap_err();
        assert!(matches!(error, ScaffoldError::AttachRejected(found) if found == lifecycle), "{state}");
        assert!(error.is_rejection());
        assert_eq!(captured.await.unwrap().len(), 1);
    }
}

/// A launch the sandbox ran as a generic exec, or one with no supervised
/// run, or one it couldn't spawn: never replayed, and the bootstrap still
/// removed, once.
#[tokio::test]
async fn an_unsupervised_or_failed_launch_is_never_replayed() {
    for (receipt, expected) in [
        (json!({"ok": true, "runId": "generic-exec"}), "contract"),
        (json!({"ok": true, "runId": "generic-exec", "supervised": false}), "contract"),
        (json!({"ok": true, "supervised": true}), "invalid"),
        (json!({"ok": true, "runId": " ", "supervised": true}), "invalid"),
        (json!({"ok": false, "error": "exec_spawn_failed"}), "missing"),
    ] {
        let expires_at = now_ms() + 60_000;
        let (origin, captured) = mock_steps(vec![
            ok(sandbox("ready")),
            ok(r#"{"ok":false,"exitCode":1}"#),
            ok(grant_answer(expires_at)),
            ok(r#"{"ok":true}"#),
            ok(r#"{"ok":true,"exitCode":0}"#),
            ok(receipt.to_string()),
            ok(r#"{"ok":true,"exitCode":0}"#),
        ])
        .await;
        let error = hosts(&origin, "t").attach(SANDBOX, &scope(), &CancellationToken::new()).await.unwrap_err();
        match expected {
            "contract" => assert!(matches!(error, ScaffoldError::RuntimeContractMismatch), "{receipt}"),
            "missing" => assert!(matches!(error, ScaffoldError::CometNotInstalled), "{receipt}"),
            _ => assert!(matches!(error, ScaffoldError::InvalidResponse(_)), "{receipt}"),
        }
        let requests = captured.await.unwrap();
        let execs = execs(&requests);
        assert_eq!(execs.iter().filter(|exec| exec["mode"] == "background").count(), 1);
        assert_eq!(execs.last().unwrap()["argv"], json!(["rm", "-f", "--", execs[2]["argv"][7]]));
        assert_eq!(requests.iter().filter(|request| request.contains("/auth/device-grants ")).count(), 1);
    }
}

/// After a launch, only the probe's exact answer confirms it: another
/// grant (an expired one, say) is final, and never returned.
#[tokio::test]
async fn attach_never_returns_an_unconfirmed_grant() {
    let expires_at = now_ms() + 60_000;
    let (origin, captured) = mock_steps(vec![
        ok(sandbox("ready")),
        ok(r#"{"ok":false,"exitCode":1}"#),
        ok(grant_answer(expires_at)),
        ok(r#"{"ok":true}"#),
        ok(r#"{"ok":true,"exitCode":0}"#),
        ok(r#"{"ok":true,"runId":"run-a","supervised":true}"#),
        ok(host_authority(GRANT, now_ms() - 1)),
        ok(r#"{"ok":true,"exitCode":0}"#),
    ])
    .await;
    let error = hosts(&origin, "t").attach(SANDBOX, &scope(), &CancellationToken::new()).await.unwrap_err();
    assert!(matches!(error, ScaffoldError::InvalidResponse(_)), "{error}");
    captured.await.unwrap();
}

/// An upload whose answer never completes may still have landed: the file
/// is removed all the same, and nothing is launched.
#[tokio::test]
async fn an_uncertain_upload_is_cleaned_up_and_not_launched() {
    let expires_at = now_ms() + 60_000;
    let cancellation = CancellationToken::new();
    for step in [Mock::HeadersThenClose, Mock::CancelThenClose(cancellation.clone())] {
        let (origin, captured) = mock_steps(vec![
            ok(sandbox("ready")),
            ok(r#"{"ok":false,"exitCode":1}"#),
            ok(grant_answer(expires_at)),
            step,
            status(503, r#"{"error":"cleanup_unavailable"}"#),
        ])
        .await;
        let result = tokio::time::timeout(
            Duration::from_secs(12),
            hosts(&origin, "t").attach(SANDBOX, &scope(), &cancellation),
        )
        .await
        .expect("bounded");
        assert!(matches!(result, Err(ScaffoldError::Transport { .. } | ScaffoldError::Cancelled)), "{result:?}");
        let requests = captured.await.unwrap();
        let execs = execs(&requests);
        assert_eq!(execs.iter().filter(|exec| exec["mode"] == "background").count(), 0);
        assert_eq!(execs.last().unwrap()["argv"][0], "rm");
    }
}

/// The Edge's refusal keeps its status and a safe code, never its body.
#[tokio::test]
async fn a_grant_refusal_keeps_only_a_safe_code() {
    for (body, code) in [
        (r#"{"error":"grant_target_forbidden","message":"Bearer secret","grant":"cg1.private"}"#, "grant_target_forbidden"),
        (r#"{"error":"Bearer private-token"}"#, "device_join_grant_rejected"),
    ] {
        let (origin, captured) = mock_steps(vec![status(403, body)]).await;
        let client = EdgeDeviceJoinGrantClient::new(&origin, bearer("sc_rc_secret")).unwrap();
        let request = DeviceJoinGrantRequest {
            principal_subject: "alice@example.com".into(),
            scope: scope(),
            sandbox_id: SANDBOX.into(),
            device_id: DEVICE.into(),
            lifecycle_epoch: 1,
            capabilities: host_capabilities(),
            expires_in_seconds: JOIN_GRANT_TTL_SECONDS,
        };
        let error = client.mint(&request, &CancellationToken::new()).await.unwrap_err();
        assert_eq!(error.api_error().map(|error| (error.status, error.code.as_str())), Some((403, code)));
        assert!(!error.to_string().contains("secret") && !error.to_string().contains("private"));
        captured.await.unwrap();
    }
}

/// A failed sandbox's last start or resume, as its chat says it: Platform's
/// message (bounded), only while it's failed, and never published with it.
/// A record of a shape this build doesn't expect costs only the words.
#[test]
fn a_failed_sandbox_says_why_in_platform_s_words() {
    let environment = |status: &str, lifecycle_error: Value| {
        let mut value = sandbox_value(status);
        value["lifecycleError"] = lifecycle_error;
        serde_json::from_value::<Sandbox>(value).unwrap().into_environment(&scope()).unwrap()
    };
    let start = json!({"action": "start", "phase": "prepare", "message": "missing session operation response",
        "occurredAt": "2026-11-22T16:07:25.884Z"});
    let failed = environment("failed", start.clone());
    assert_eq!(failed.failure.as_deref(), Some("The sandbox didn't start: missing session operation response."));
    let published = serde_json::to_value(&failed).unwrap();
    assert!(!published.to_string().contains("missing session"), "never published: {published}");
    assert_eq!(serde_json::from_value::<SessionEnvironment>(published).unwrap().failure, None);

    assert_eq!(environment("ready", start).failure, None, "only while it's failed");
    assert_eq!(
        environment("failed", json!({"action": "resume", "message": "base copy failed."})).failure.as_deref(),
        Some("The sandbox didn't resume: base copy failed.")
    );
    let long = environment("failed", json!({"action": "start", "message": "x".repeat(400)})).failure.unwrap();
    assert!(long.ends_with("x…") && long.chars().count() < 340, "{long}");
    for odd in [json!("an error"), json!({"action": "start"}), json!({"message": "  "}), json!(null)] {
        assert_eq!(environment("failed", odd.clone()).failure, None, "{odd}");
    }
    let mut without = sandbox_value("failed");
    without.as_object_mut().unwrap().remove("lifecycleError");
    assert_eq!(serde_json::from_value::<Sandbox>(without).unwrap().into_environment(&scope()).unwrap().failure, None);
}

#[test]
fn a_host_s_device_names_its_sandbox_and_epoch() {
    assert_eq!(parse_device_id(DEVICE), Some((SANDBOX, 1)));
    assert_eq!(parse_device_id("comet-scaffold-rcs_a1-b2-e12"), Some(("rcs_a1-b2", 12)));
    for device in ["comet-scaffold-sandbox-a", "comet-scaffold--e1", "comet-scaffold-a-e0", "comet-scaffold-a-ex", "cloud-sandbox-e1", "comet-scaffold-a b-e1"] {
        assert_eq!(parse_device_id(device), None, "{device}");
    }
}
