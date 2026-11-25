//! The Cloud adapter against a fake Platform and a fake Edge (one loopback
//! origin, answering by route), a real workspace and ledger: no network.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use zeron_proto::{ChatConfig, HarnessId};
use zeron_rpc::{RpcReply, StaticToken, TokenSource};
use zeron_sync::DocsStore;

use super::scaffold::tests::{DEVICE, GRANT, SANDBOX, SESSION, host_authority, sandbox, sandbox_value};
use super::*;
use crate::workspace_host::WorkspaceHostConfig;

const EDGE: &str = "https://crew-zgx-edge.example";

/// A fake Platform and Edge: answers each request by what `respond` says,
/// for as long as the test runs, keeping every request.
async fn serve(respond: impl Fn(&str, &Value) -> (u16, String) + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let respond = Arc::new(respond);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let seen = seen.clone();
            let respond = respond.clone();
            tokio::spawn(async move {
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
                            .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                if bytes.is_empty() {
                    return;
                }
                let request = String::from_utf8(bytes).unwrap();
                let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").map_or("", |(_, body)| body)).unwrap_or(Value::Null);
                let (status, answer) = respond(&request, &body);
                seen.lock().unwrap().push(request);
                let _ = stream
                    .write_all(
                        format!("HTTP/1.1 {status} Answer\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}", answer.len())
                            .as_bytes(),
                    )
                    .await;
            });
        }
    });
    (origin, requests)
}

/// Platform with one sandbox for [`SESSION`] (in `state`, made by the first
/// create), whose host answers the probe; `create` says what a create gets.
fn platform(create: (u16, String), state: &'static str) -> impl Fn(&str, &Value) -> (u16, String) + Send + Sync + 'static {
    let created = Arc::new(Mutex::new(false));
    move |request, body| {
        let line = request.lines().next().unwrap_or_default();
        if line.starts_with("POST /api/code-sandboxes ") {
            *created.lock().unwrap() = create.0 < 300;
            create.clone()
        } else if line.starts_with("GET /api/code-sandboxes ") {
            let listed: Vec<Value> = if *created.lock().unwrap() { vec![sandbox_value(state)] } else { Vec::new() };
            (200, json!({"ok": true, "sandboxes": listed}).to_string())
        } else if line.starts_with(&format!("GET /api/code-sandboxes/{SANDBOX} ")) {
            (200, sandbox(state))
        } else if line.starts_with(&format!("POST /api/code-sandboxes/{SANDBOX}/exec ")) && body["argv"][5] == "scaffold-authority" {
            (200, host_authority(GRANT, crate::now_ms() + 3_600_000))
        } else {
            (404, r#"{"error":"not_found"}"#.into())
        }
    }
}

fn controller() -> Controller {
    Controller {
        subject: "alice@example.com".into(),
        email: "alice@example.com".into(),
        project: "project-a".into(),
        deployment: "deployment-a".into(),
    }
}

struct Fixture {
    cloud: Arc<CrewCloud>,
    workspace: WorkspaceHost,
    store: Arc<DocsStore>,
    _dir: tempfile::TempDir,
}

fn fixture(origin: &str, region: Option<&str>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("store")).unwrap();
    let store = Arc::new(DocsStore::open(&dir.path().join("store")).unwrap());
    let workspace = WorkspaceHost::open(
        store.clone(),
        WorkspaceHostConfig {
            device_id: "desktop".into(),
            device_name: "Desktop".into(),
            platform: "macos".into(),
            org_id: "project-a".into(),
            user_id: "alice@example.com".into(),
            edge: None,
            local_only: false,
        },
    )
    .unwrap();
    let bearer: Arc<dyn TokenSource> = Arc::new(StaticToken("sc_rc_owner".into()));
    let client = ScaffoldClient::new(origin, "project-a", bearer.clone()).unwrap();
    let grants = Arc::new(EdgeDeviceJoinGrantClient::new(origin, bearer).unwrap());
    let hosts = SandboxHosts::new(client, EDGE, grants).unwrap();
    let ledger = Ledger::open(&dir.path().join("store")).unwrap();
    let cloud = Arc::new(CrewCloud::from_parts(
        Identity::Fixed(controller()),
        workspace.clone(),
        hosts,
        region.map(str::to_owned),
        Some("deployment-a".into()),
        ledger,
    ));
    Fixture { cloud, workspace, store, _dir: dir }
}

fn omp(model: &str, account: Option<&str>) -> ChatConfig {
    let mut config: ChatConfig = serde_json::from_value(json!({
        "harness": "omp", "model": model, "reasoning": null, "modelOptions": {}, "sandbox": "workspace-write",
    }))
    .unwrap();
    if let Some(account) = account {
        config.model_options.insert("crewAccountId".into(), json!(account));
    }
    config
}

fn count(requests: &Arc<Mutex<Vec<String>>>, prefix: &str) -> usize {
    requests.lock().unwrap().iter().filter(|request| request.starts_with(prefix)).count()
}

/// The ledger's outcome for `id`, once there is one (the attach runs in the
/// background).
async fn settled(cloud: &CrewCloud, id: &str) -> Outcome {
    for _ in 0..200 {
        if let Some(outcome) = cloud.settled(id).unwrap() {
            return outcome;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{id} never settled");
}

/// The exit gate's desktop half: a Cloud chat creates its sandbox in this
/// engine's region for the chat (session id = chat id), its row goes on the
/// sandbox's host as the Edge admits it, and the host is attached (here: an
/// exact one already serving, reused). Asking again changes nothing.
#[tokio::test]
async fn a_cloud_chat_creates_its_sandbox_publishes_its_row_and_attaches_its_host() {
    let (origin, requests) = serve(platform((202, sandbox("starting")), "starting")).await;
    let Fixture { cloud, workspace, .. } = fixture(&origin, Some("harness-zgx"));
    let config = omp("anthropic/claude-opus-5-5", Some("account-1"));
    let (device, cwd) = cloud.chat_host(SESSION, Some("master"), Some(&config)).await.unwrap();
    assert_eq!((device.as_str(), cwd.as_str()), (DEVICE, "/workspace/ashler-platform"));

    let create = requests.lock().unwrap()[0].clone();
    let body: Value = serde_json::from_str(create.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert!(create.starts_with("POST /api/code-sandboxes HTTP"));
    assert_eq!(
        body,
        json!({"region": "harness-zgx", "databaseEnvironment": "local",
            "agentRoute": {"provider": "anthropic", "model": "claude-opus-5-5", "fallback": "disabled",
                "routingMode": "pinned", "accountId": "account-1"},
            "cometRuntimeProfile": {"version": "scaffold.comet-runtime.v1", "projectId": "project-a",
                "deploymentId": "deployment-a", "sessionId": SESSION}})
    );

    // The row, as the Edge's bindingMatches reads it.
    let chat = workspace.chat(SESSION).unwrap().unwrap();
    assert_eq!((chat.device_id.as_str(), chat.space_id.as_deref(), chat.room_gen), (DEVICE, None, Some(2)));
    assert_eq!(chat.cwd.as_deref(), Some("/workspace/ashler-platform"));
    let config = chat.config.unwrap();
    assert_eq!((config.harness, config.model.as_deref()), (HarnessId::Omp, Some("anthropic/claude-opus-5-5")));
    assert_eq!(config.model_options.get("crewAccountId"), Some(&json!("account-1")));
    let crew = workspace.crew_chat_metadata(SESSION).unwrap();
    let scope = json!({"projectId": "project-a", "deploymentId": "deployment-a", "sessionId": SESSION});
    assert_eq!(crew["ownerSubject"], "alice@example.com");
    assert_eq!(crew["sessionId"], SESSION);
    assert_eq!(crew["scope"], scope);
    assert_eq!(crew["environment"]["ownerPrincipal"], "alice@example.com");
    assert_eq!(crew["environment"]["scope"], scope);
    assert_eq!(crew["environment"]["source"]["kind"], "scaffold");
    assert_eq!(crew["environment"]["source"]["sandbox_id"], SANDBOX);
    assert_eq!(crew["environment"]["source"]["lifecycle_epoch"], 1);
    assert_eq!(crew["environment"]["source"]["lifecycle"], "starting");
    assert_eq!(crew["agentRoute"]["accountId"], "account-1");
    let host = workspace.read_devices().unwrap().into_iter().find(|device| device.id == DEVICE).unwrap();
    assert_eq!((host.platform.as_str(), host.capabilities.as_slice()), ("linux", &["cloud-session".to_owned()][..]));

    // Attached in the background: an exact host already served.
    let attach = settled(&cloud, &format!("attach:{SANDBOX}:e1")).await;
    assert_eq!(attach.status, "confirmed");
    assert_eq!(attach.attachment.unwrap().grant.id, GRANT);
    assert_eq!(settled(&cloud, &format!("create:{SESSION}")).await.status, "confirmed");
    assert_eq!(count(&requests, "POST /auth/device-grants"), 0, "a reused host mints nothing");

    // The same chat again: the same host, nothing created.
    let again = cloud.chat_host(SESSION, None, Some(&omp("anthropic/claude-opus-5-5", Some("account-1")))).await.unwrap();
    assert_eq!(again.0, DEVICE);
    assert_eq!(count(&requests, "POST /api/code-sandboxes HTTP"), 1);
}

/// With no sandbox region named, nothing is created: never in a region
/// whose Edge isn't this engine's. Nor from another branch, without a
/// model of OMP's, or for a chat id that isn't Scaffold's.
#[tokio::test]
async fn without_a_region_or_a_model_nothing_is_created() {
    let (origin, requests) = serve(platform((202, sandbox("starting")), "starting")).await;
    let unset = fixture(&origin, None);
    let config = omp("anthropic/claude-opus-5-5", None);
    let refused = unset.cloud.chat_host(SESSION, None, Some(&config)).await.unwrap_err().to_string();
    assert!(refused.contains("sandboxRegion"), "{refused}");
    assert!(!unset.cloud.status().available, "the Cloud row hides without a region");
    let set = fixture(&origin, Some("harness-zgx"));
    assert!(set.cloud.status().available);
    for (branch, config, chat) in [
        (Some("feature"), Some(omp("anthropic/claude-opus-5-5", None)), SESSION),
        (None, None, SESSION),
        (None, Some(omp("claude-opus-5-5", None)), SESSION),
        (None, Some(omp("anthropic/claude-opus-5-5", None)), "Not-A-UUID"),
    ] {
        assert!(set.cloud.chat_host(chat, branch, config.as_ref()).await.is_err(), "{branch:?} {chat}");
    }
    assert_eq!(requests.lock().unwrap().len(), 0, "nothing was sent");
}

/// A create whose answer never came isn't sent again: the next ask lists
/// Platform's sandboxes for the chat's session, and settles it from what's
/// there (or says it's still looking).
#[tokio::test]
async fn a_lost_create_is_found_by_its_session_and_never_sent_again() {
    let created = Arc::new(Mutex::new(false));
    let made = created.clone();
    let (origin, requests) = serve(move |request, body| {
        let line = request.lines().next().unwrap_or_default();
        if line.starts_with("POST /api/code-sandboxes ") {
            // Platform made it; the answer was lost.
            *made.lock().unwrap() = true;
            (502, r#"{"error":"bad_gateway"}"#.into())
        } else if line.starts_with("GET /api/code-sandboxes ") {
            let listed: Vec<Value> = if *made.lock().unwrap() { vec![sandbox_value("ready")] } else { Vec::new() };
            (200, json!({"ok": true, "sandboxes": listed}).to_string())
        } else if line.starts_with(&format!("GET /api/code-sandboxes/{SANDBOX} ")) {
            (200, sandbox("ready"))
        } else if line.starts_with(&format!("POST /api/code-sandboxes/{SANDBOX}/exec ")) && body["argv"][5] == "scaffold-authority" {
            (200, host_authority(GRANT, crate::now_ms() + 3_600_000))
        } else {
            (404, "{}".into())
        }
    })
    .await;
    let Fixture { cloud, workspace, .. } = fixture(&origin, Some("harness-zgx"));
    let config = omp("openai/gpt-6.2-sol", None);
    let lost = cloud.chat_host(SESSION, None, Some(&config)).await.unwrap_err().to_string();
    assert!(lost.contains("won't create another"), "{lost}");
    assert!(workspace.chat(SESSION).unwrap().is_none());
    assert_eq!(cloud.inner.ledger.pending("", 64).unwrap().len(), 1, "unknown, kept to observe");
    // Another model for the same chat is the same command asked otherwise.
    assert!(cloud.chat_host(SESSION, None, Some(&omp("anthropic/claude-opus-5-5", None))).await.is_err());

    let (device, _) = cloud.chat_host(SESSION, None, Some(&config)).await.unwrap();
    assert_eq!(device, DEVICE);
    assert_eq!(count(&requests, "POST /api/code-sandboxes HTTP"), 1, "sent once, ever");
    assert_eq!(count(&requests, "GET /api/code-sandboxes HTTP"), 1, "found by listing");
    assert_eq!(settled(&cloud, &format!("create:{SESSION}")).await.status, "confirmed");
    assert_eq!(workspace.chat(SESSION).unwrap().unwrap().device_id, DEVICE);
    assert_eq!(settled(&cloud, &format!("attach:{SANDBOX}:e1")).await.status, "confirmed");
}

/// A 4xx is a rejection, settled once: asking again gives the same answer
/// and sends nothing.
#[tokio::test]
async fn a_rejected_create_is_settled_and_not_sent_again() {
    let refusal = (400, r#"{"error":"remote_code_region_not_allowed","message":"Region harness-zgx is not allowed"}"#.to_owned());
    let (origin, requests) = serve(platform(refusal, "starting")).await;
    let Fixture { cloud, .. } = fixture(&origin, Some("harness-zgx"));
    let config = omp("anthropic/claude-opus-5-5", None);
    let first = cloud.chat_host(SESSION, None, Some(&config)).await.unwrap_err().to_string();
    assert!(first.contains("Region harness-zgx is not allowed") && first.contains("(400)"), "{first}");
    let second = cloud.chat_host(SESSION, None, Some(&config)).await.unwrap_err().to_string();
    assert_eq!(first, second);
    assert_eq!(count(&requests, "POST /api/code-sandboxes HTTP"), 1);
    assert_eq!(cloud.settled(&format!("create:{SESSION}")).unwrap().unwrap().status, "rejected");
}

/// Reconcile only observes: a pending Attach is settled by the host's probe
/// (no grant, no file, no launch), and this region's sandboxes get rows.
#[tokio::test]
async fn reconcile_observes_and_never_sends_again() {
    let (origin, requests) = serve(platform((202, sandbox("ready")), "ready")).await;
    let Fixture { cloud, workspace, .. } = fixture(&origin, Some("harness-zgx"));
    // An Attach claimed by a process that died before its outcome.
    let attach = format!("attach:{SANDBOX}:e1");
    let request = json!({"principal": "alice@example.com", "scope": {"projectId": "project-a", "deploymentId": "deployment-a",
        "sessionId": SESSION}, "sandboxId": SANDBOX, "lifecycleEpoch": 1});
    cloud.inner.ledger.claim(&attach, &serde_json::to_vec(&request).unwrap()).unwrap();
    // Platform knows the sandbox (another of this desktop's asks made it).
    let made = cloud
        .inner
        .hosts
        .client()
        .create(
            &controller().scope(SESSION),
            CreateOptions { region: Some("harness-zgx"), ..Default::default() },
            &AgentRoute::automatic(scaffold::AgentProvider::Anthropic, "claude-opus-5-5"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(made.region(), Some("harness-zgx"));
    let before = requests.lock().unwrap().len();
    cloud.reconcile_all().await.unwrap();
    let outcome = cloud.settled(&attach).unwrap().expect("settled by observation");
    assert_eq!((outcome.status.as_str(), outcome.attachment.unwrap().device_id.as_str()), ("confirmed", DEVICE));
    let after: Vec<String> = requests.lock().unwrap()[before..].to_vec();
    assert!(after.iter().all(|request| !request.contains("/auth/device-grants")
        && !request.contains("--device-bootstrap-file")
        && !request.starts_with("POST /api/code-sandboxes HTTP")
        && !request.starts_with("PUT ")), "{after:#?}");
    assert_eq!(workspace.chat(SESSION).unwrap().unwrap().device_id, DEVICE, "the refresh published its row");
}

/// The Cloud RPCs: ready on the logical device, Ashler-AI/ashler-platform on
/// its default branch, this region's sandboxes as sessions; everything else
/// refused in a sentence.
#[tokio::test]
async fn the_cloud_rpcs_answer_as_crew_s_sandboxes() {
    let other_region = {
        let mut other = sandbox_value("ready");
        other["region"] = json!("default");
        other["id"] = json!("sandbox-b");
        other["cometRuntimeProfile"]["sandboxId"] = json!("sandbox-b");
        other["cometRuntimeProfile"]["sessionId"] = json!("9dd173da-fb53-48df-9581-026009c783e2");
        other
    };
    let (origin, _) = serve(move |request, _| {
        if request.starts_with("GET /api/code-sandboxes ") {
            (200, json!({"ok": true, "sandboxes": [sandbox_value("paused"), other_region.clone()]}).to_string())
        } else {
            (404, "{}".into())
        }
    })
    .await;
    let Fixture { cloud, workspace, .. } = fixture(&origin, Some("harness-zgx"));
    let value = |reply: Option<Result<RpcReply, RpcError>>| match reply.expect("answered").expect("not refused") {
        RpcReply::Value(value) => value,
        RpcReply::Stream(_) => panic!("a value"),
    };
    let status = value(cloud.cloud_rpc(methods::CLOUD_STATUS, &json!({})).await);
    assert_eq!((status["state"].clone(), status["available"].clone(), status["deviceId"].clone()), (json!("ready"), json!(true), json!("cloud")));
    let repos = value(cloud.cloud_rpc(methods::LIST_GITHUB_REPOS, &json!({})).await);
    assert_eq!(repos["repos"].as_array().unwrap().len(), 1);
    assert_eq!((repos["repos"][0]["fullName"].clone(), repos["repos"][0]["defaultBranch"].clone()), (json!("Ashler-AI/ashler-platform"), json!("master")));
    let branches = value(cloud.cloud_rpc(methods::LIST_CLOUD_BRANCHES, &json!({"repo": "ashler-ai/ashler-platform"})).await);
    assert_eq!(branches, json!([{"name": "master", "current": true}]));
    assert!(cloud.cloud_rpc(methods::LIST_CLOUD_BRANCHES, &json!({"repo": "acme/atlas"})).await.unwrap().is_err());
    let sessions = value(cloud.cloud_rpc(methods::CLOUD_SESSIONS, &json!({})).await);
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 1, "this region's only");
    assert_eq!(
        (sessions["sessions"][0]["chatId"].clone(), sessions["sessions"][0]["deviceId"].clone(), sessions["sessions"][0]["state"].clone()),
        (json!(SESSION), json!(DEVICE), json!("sleeping"))
    );
    assert_eq!(workspace.chat(SESSION).unwrap().unwrap().device_id, DEVICE, "listing publishes rows, nothing more");
    for method in [
        methods::CLOUD_SESSION_SLEEP,
        methods::CLOUD_SESSION_DELETE,
        methods::CLOUD_SET_MACHINE,
        methods::CLOUD_SET_REACH,
        methods::BILLING_STATUS,
    ] {
        let refused = cloud.cloud_rpc(method, &json!({})).await.unwrap().err().expect("refused").to_string();
        assert!(refused.contains("Crew's sandboxes"), "{method}: {refused}");
    }
    assert!(cloud.cloud_rpc(methods::VAULT_STATUS, &json!({})).await.is_none(), "upstream's, unavailable");
    for (lifecycle, state) in [
        (Lifecycle::Creating, CloudState::Provisioning),
        (Lifecycle::Starting, CloudState::Starting),
        (Lifecycle::Ready, CloudState::Ready),
        (Lifecycle::Paused, CloudState::Sleeping),
        (Lifecycle::Failed, CloudState::Error),
    ] {
        assert_eq!(cloud_state(lifecycle), state);
    }
}

/// A failed sandbox's session says why (Cloud's "Machine failed" alert), in
/// Platform's words when it recorded some, and the way on; its Try again is
/// refused saying the same. A working one says nothing.
#[tokio::test]
async fn a_failed_sandbox_s_session_says_why_and_the_way_on() {
    let other = |id: &str, session: &str, status: &str| {
        let mut value = sandbox_value(status);
        value["id"] = json!(id);
        value["cometRuntimeProfile"]["sandboxId"] = json!(id);
        value["cometRuntimeProfile"]["sessionId"] = json!(session);
        value
    };
    let mut recorded = sandbox_value("failed");
    recorded["lifecycleError"] = json!({"action": "start", "phase": "prepare", "message": "missing session operation response"});
    // A launch refused before its runtime ran (omp_inference_access_required): no record.
    let unrecorded = other("sandbox-b", "9dd173da-fb53-48df-9581-026009c783e2", "failed");
    let ready = other("sandbox-c", "5f0c1a52-7a3e-4c55-9d1e-7b8f1c2d3e4f", "ready");
    let (origin, _) = serve(move |request, _| {
        if request.starts_with("GET /api/code-sandboxes ") {
            (200, json!({"ok": true, "sandboxes": [recorded.clone(), unrecorded.clone(), ready.clone()]}).to_string())
        } else {
            (404, "{}".into())
        }
    })
    .await;
    let Fixture { cloud, .. } = fixture(&origin, Some("harness-zgx"));
    let Some(Ok(RpcReply::Value(sessions))) = cloud.cloud_rpc(methods::CLOUD_SESSIONS, &json!({})).await else {
        panic!("a value");
    };
    let said: Vec<_> = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| (session["chatId"].as_str().unwrap().to_owned(), session["state"].clone(), session["error"].clone()))
        .collect();
    assert_eq!(
        said,
        vec![
            (
                SESSION.to_owned(),
                json!("error"),
                json!("The sandbox didn't start: missing session operation response. Start a new chat to try again.")
            ),
            ("9dd173da-fb53-48df-9581-026009c783e2".to_owned(), json!("error"), json!("Its sandbox failed. Start a new chat to try again.")),
            ("5f0c1a52-7a3e-4c55-9d1e-7b8f1c2d3e4f".to_owned(), json!("ready"), Value::Null),
        ]
    );
    let retry = cloud.cloud_rpc(methods::CLOUD_SESSION_RETRY, &json!({})).await.unwrap().err().expect("refused").to_string();
    assert!(retry.contains("Crew's sandboxes") && retry.contains("Start a new chat to try again."), "{retry}");
}

/// A failed sandbox's host never claims its device again (Crew sends no
/// Start), so a wake owed to its chat settles with no nudge. Before, the chat
/// nudged the Edge every few seconds (each a 404) for as long as its message
/// waited. A working sandbox's chat is still woken.
#[tokio::test]
async fn a_failed_sandbox_s_chat_stops_waking_its_host() {
    use crate::doc_host::{DocHost, DocHostConfig, EdgeConfig};
    let (origin, requests) = serve(|_, _| (404, "{}".into())).await;
    let Fixture { cloud, workspace, store, .. } = fixture(&origin, Some("harness-zgx"));
    let environment = |state: &str| {
        serde_json::from_value::<scaffold::Sandbox>(sandbox_value(state)).unwrap().into_environment(&controller().scope(SESSION)).unwrap()
    };
    let nudge = format!("POST /device/{DEVICE}/nudge ");
    let nudges = || requests.lock().unwrap().iter().filter(|request| request.starts_with(&nudge)).count();
    let settled = || async {
        tokio::time::timeout(Duration::from_secs(30), async {
            while store.sync_job_version(SESSION, "remote-wake").unwrap().is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    };

    assert!(cloud.publish(&controller(), &environment("ready"), None, None).unwrap());
    assert!(!sandbox_retired(&workspace, SESSION, DEVICE), "a working sandbox's host");
    let host = DocHost::new(
        store.clone(),
        DocHostConfig { device_id: "desktop".into(), default_harness: HarnessId::Mock, edge: Some(EdgeConfig::with_static_token(&origin, "test")) },
    );
    host.set_workspace(workspace.clone());
    store.schedule_sync_job(SESSION, "remote-wake").unwrap();
    let woken = tokio::time::timeout(Duration::from_secs(10), async {
        while nudges() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(woken.is_ok(), "a working sandbox's host is woken");

    assert!(cloud.publish(&controller(), &environment("failed"), None, None).unwrap());
    assert!(sandbox_retired(&workspace, SESSION, DEVICE));
    assert!(!sandbox_retired(&workspace, SESSION, "comet-scaffold-sandbox-a-e2"), "another epoch's host");
    assert!(!sandbox_retired(&workspace, SESSION, "cloud-machine-1"), "not a sandbox's host");
    assert!(settled().await, "the wake is settled");
    let before = nudges();
    // Owed again (a later send's nudge): settled again, with nothing sent.
    store.schedule_sync_job(SESSION, "remote-wake").unwrap();
    assert!(settled().await, "settled again");
    assert_eq!(nudges(), before, "no nudge for a failed sandbox's host");
    host.shutdown_workers().await;
}

/// A send to a paused sandbox's chat resumes it (Crew main #237): the 404 its
/// host's nudge gets wakes the sandbox once, its new epoch's row is published,
/// and the waiting send goes to that epoch's host. Cloud's Wake does the same
/// and never resumes it twice; a failed one is refused in its alert's words.
#[tokio::test]
async fn a_send_to_a_paused_sandbox_resumes_it() {
    use std::sync::atomic::{AtomicU8, Ordering};

    use crate::doc_host::{DocHost, DocHostConfig, EdgeConfig};
    fn answer(status: &str, epoch: u64) -> String {
        let mut value = sandbox_value(status);
        value["lifecycleEpoch"] = json!(epoch);
        json!({"ok": true, "sandbox": value}).to_string()
    }
    // Platform's sandbox: paused at epoch 1, resuming at 2 once asked, then failed.
    let phase = Arc::new(AtomicU8::new(0));
    let platform = phase.clone();
    let (origin, requests) = serve(move |request, _| {
        if request.starts_with("POST /api/code-sandboxes/sandbox-a/resume ") {
            platform.store(1, Ordering::SeqCst);
            return (200, answer("resuming", 2));
        }
        if request.starts_with("GET /api/code-sandboxes/sandbox-a ") {
            return match platform.load(Ordering::SeqCst) {
                0 => (200, answer("paused", 1)),
                1 => (200, answer("resuming", 2)),
                _ => (200, answer("failed", 2)),
            };
        }
        (404, "{}".into()) // no host serving yet: nudges, probes and grants
    })
    .await;
    let Fixture { cloud, workspace, store, .. } = fixture(&origin, Some("harness-zgx"));
    let resumes = || requests.lock().unwrap().iter().filter(|request| request.starts_with("POST /api/code-sandboxes/sandbox-a/resume ")).count();
    let new_host = "comet-scaffold-sandbox-a-e2";
    let paused = serde_json::from_value::<scaffold::Sandbox>(sandbox_value("paused"))
        .unwrap()
        .into_environment(&controller().scope(SESSION))
        .unwrap();
    assert!(cloud.publish(&controller(), &paused, None, None).unwrap());
    assert_eq!(workspace.chat(SESSION).unwrap().unwrap().device_id, DEVICE);

    let host = DocHost::new(
        store.clone(),
        DocHostConfig { device_id: "desktop".into(), default_harness: HarnessId::Mock, edge: Some(EdgeConfig::with_static_token(&origin, "test")) },
    );
    host.set_workspace(workspace.clone());
    host.set_crew_cloud(&cloud);
    store.schedule_sync_job(SESSION, "remote-wake").unwrap();
    let moved = tokio::time::timeout(Duration::from_secs(20), async {
        while workspace.chat(SESSION).unwrap().unwrap().device_id != new_host {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(moved.is_ok(), "the send resumed its sandbox, and its row moved to the new epoch's host");
    assert_eq!(resumes(), 1);
    let nudge = format!("POST /device/{new_host}/nudge ");
    let retargeted = tokio::time::timeout(Duration::from_secs(20), async {
        while !requests.lock().unwrap().iter().any(|request| request.starts_with(&nudge)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(retargeted.is_ok(), "the waiting send goes to the new epoch's host");

    // Cloud's Wake while it resumes: its session, and no second resume.
    let Some(Ok(RpcReply::Value(session))) = cloud.cloud_rpc(methods::CLOUD_SESSION_WAKE, &json!({"chatId": SESSION})).await else {
        panic!("a woken session");
    };
    assert_eq!((session["deviceId"].clone(), session["state"].clone()), (json!(new_host), json!("starting")));
    assert_eq!(resumes(), 1, "never resumed twice");

    // A failed one: refused in its alert's words, and nothing resumed.
    phase.store(2, Ordering::SeqCst);
    let refused = cloud.cloud_rpc(methods::CLOUD_SESSION_WAKE, &json!({"chatId": SESSION})).await.unwrap().err().expect("refused").to_string();
    assert!(refused.contains("Start a new chat to try again."), "{refused}");
    assert_eq!(resumes(), 1);
    host.shutdown_workers().await;
}

/// A send made before the chat's row learned of the pause: its nudge goes to
/// the last host's device room, which that host claimed, so the Edge queues it
/// (200, `delivered: false`) rather than answering 404. The sandbox is woken
/// all the same, and the chat moves to the resumed epoch's host, whose boot
/// finds the waiting send.
#[tokio::test]
async fn a_send_queued_for_a_paused_sandbox_s_last_host_resumes_it() {
    use std::sync::atomic::{AtomicU8, Ordering};

    use crate::doc_host::{DocHost, DocHostConfig, EdgeConfig};
    fn answer(status: &str, epoch: u64) -> String {
        let mut value = sandbox_value(status);
        value["lifecycleEpoch"] = json!(epoch);
        json!({"ok": true, "sandbox": value}).to_string()
    }
    let phase = Arc::new(AtomicU8::new(0));
    let platform = phase.clone();
    let (origin, requests) = serve(move |request, _| {
        if request.starts_with("POST /api/code-sandboxes/sandbox-a/resume ") {
            platform.store(1, Ordering::SeqCst);
            return (200, answer("resuming", 3));
        }
        if request.starts_with("GET /api/code-sandboxes/sandbox-a ") {
            return match platform.load(Ordering::SeqCst) {
                0 => (200, answer("paused", 2)),
                _ => (200, answer("resuming", 3)),
            };
        }
        // The last host's room, claimed when it connected: the nudge waits there.
        if request.starts_with(&format!("POST /device/{DEVICE}/nudge ")) {
            return (200, json!({"delivered": false, "queued": true}).to_string());
        }
        (404, "{}".into())
    })
    .await;
    let Fixture { cloud, workspace, store, .. } = fixture(&origin, Some("harness-zgx"));
    let resumes = || requests.lock().unwrap().iter().filter(|request| request.starts_with("POST /api/code-sandboxes/sandbox-a/resume ")).count();
    // The row as the sandbox last ran: epoch 1's host.
    let running = serde_json::from_value::<scaffold::Sandbox>(sandbox_value("ready"))
        .unwrap()
        .into_environment(&controller().scope(SESSION))
        .unwrap();
    assert!(cloud.publish(&controller(), &running, None, None).unwrap());
    assert_eq!(workspace.chat(SESSION).unwrap().unwrap().device_id, DEVICE);

    let host = DocHost::new(
        store.clone(),
        DocHostConfig { device_id: "desktop".into(), default_harness: HarnessId::Mock, edge: Some(EdgeConfig::with_static_token(&origin, "test")) },
    );
    host.set_workspace(workspace.clone());
    host.set_crew_cloud(&cloud);
    store.schedule_sync_job(SESSION, "remote-wake").unwrap();
    let resumed_host = "comet-scaffold-sandbox-a-e3";
    let moved = tokio::time::timeout(Duration::from_secs(20), async {
        while workspace.chat(SESSION).unwrap().unwrap().device_id != resumed_host {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(moved.is_ok(), "the queued send resumed its sandbox, and its row moved to the resumed epoch's host");
    assert_eq!(resumes(), 1);
    host.shutdown_workers().await;
}

/// A row is never re-homed from another device, bound to another sandbox,
/// or moved back to an older epoch.
#[tokio::test]
async fn a_published_row_only_moves_forward_on_its_own_sandbox() {
    let Fixture { cloud, workspace, .. } = fixture("http://127.0.0.1:9", Some("harness-zgx"));
    let environment = |id: &str, epoch: u64, state: &str| {
        let mut value = sandbox_value(state);
        value["id"] = json!(id);
        value["lifecycleEpoch"] = json!(epoch);
        value["cometRuntimeProfile"]["sandboxId"] = json!(id);
        serde_json::from_value::<scaffold::Sandbox>(value).unwrap().into_environment(&controller().scope(SESSION)).unwrap()
    };
    let controller = controller();
    assert!(cloud.publish(&controller, &environment(SANDBOX, 2, "ready"), None, None).unwrap());
    assert_eq!(workspace.chat(SESSION).unwrap().unwrap().device_id, "comet-scaffold-sandbox-a-e2");
    assert!(!cloud.publish(&controller, &environment(SANDBOX, 1, "ready"), None, None).unwrap(), "never back");
    assert!(!cloud.publish(&controller, &environment(SANDBOX, 2, "ready"), None, None).unwrap(), "unchanged");
    assert!(cloud.publish(&controller, &environment("sandbox-b", 3, "ready"), None, None).is_err(), "another sandbox");
    assert_eq!(workspace.chat(SESSION).unwrap().unwrap().device_id, "comet-scaffold-sandbox-a-e2");
}

/// Crew's pre-filter, on the Edge's list: a sandbox host gets only its own
/// session's typed calls; any other target passes.
#[tokio::test]
async fn only_what_the_edge_relays_goes_to_a_sandbox_host() {
    let Fixture { cloud, workspace, .. } = fixture("http://127.0.0.1:9", Some("harness-zgx"));
    let chat = json!({"chatId": SESSION});
    let with = |extra: Value| {
        let mut params = chat.clone();
        params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        params
    };
    // No row binds the host yet: nothing goes.
    assert!(guard_scaffold_target(&workspace, DEVICE, methods::WATCH_DOC_MESSAGES, &chat).is_err());
    let made = sandbox_value("ready");
    let environment = serde_json::from_value::<scaffold::Sandbox>(made).unwrap().into_environment(&controller().scope(SESSION)).unwrap();
    cloud.publish(&controller(), &environment, None, None).unwrap();
    for _ in 0..100 {
        if workspace.watch_chats().borrow().iter().any(|chat| chat.device_id == DEVICE) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for (method, params) in [
        (methods::WATCH_DOC_MESSAGES, chat.clone()),
        (methods::QUEUE_COMMAND, with(json!({"command": {"kind": "interrupt"}}))),
        (methods::QUEUE_MESSAGE, with(json!({"text": "next", "holdForTurnEnd": true}))),
        (methods::BEGIN_QUEUED_MESSAGE_EDIT, with(json!({"id": "q1"}))),
        (methods::SEND_QUEUED_MESSAGE_NOW, with(json!({"id": "q1", "targetDeviceId": DEVICE}))),
        (methods::STOP_WAITING, chat.clone()),
        (methods::UPLOAD_CHUNK, with(json!({"uploadId": "u", "seq": 0, "data": ""}))),
    ] {
        assert!(guard_scaffold_target(&workspace, DEVICE, method, &params).is_ok(), "{method}");
    }
    for (method, params) in [
        (methods::LIST_HARNESSES, json!({"targetDeviceId": DEVICE})),
        (methods::DEVICE_STATS, json!({"targetDeviceId": DEVICE})),
        (methods::OPEN_TERMINAL, chat.clone()),
        (methods::READ_WORKSPACE_FILE, chat.clone()),
        (methods::UPDATE_STATUS, json!({})),
        (methods::UPLOAD_CHUNK, json!({"uploadId": "u", "seq": 0, "data": ""})),
        (methods::QUEUE_COMMAND, with(json!({"command": {"kind": "reset"}}))),
        // Never a relayed command, typed or not (Crew main #231): the chat doc is
        // a sandbox host's only road for one.
        (methods::RELAY_COMMAND, with(json!({"entry": {"payload": {"kind": "run"}}}))),
        (methods::RELAY_COMMAND, with(json!({"entry": {"payload": {"kind": "reset"}}}))),
        (methods::WATCH_DOC_MESSAGES, json!({"chatId": "another-chat"})),
    ] {
        let refused = guard_scaffold_target(&workspace, DEVICE, method, &params).unwrap_err().to_string();
        assert!(refused.contains("session-scoped Scaffold host"), "{method}: {refused}");
    }
    // Not a sandbox host: as upstream.
    assert!(guard_scaffold_target(&workspace, "laptop-1", methods::LIST_HARNESSES, &json!({})).is_ok());
    assert!(guard_scaffold_target(&workspace, "cloud-abc", methods::OPEN_TERMINAL, &json!({})).is_ok());
    // The command escort relays to no sandbox's host; any other host keeps the road.
    assert!(!relay_admitted(&workspace, SESSION));
    workspace.create_chat("laptop-chat", None, Some("laptop-1"), None, None).unwrap();
    assert!(relay_admitted(&workspace, "laptop-chat"));
    assert!(relay_admitted(&workspace, "no-such-chat"));
}

/// The engine's seams (rpc.rs): its Cloud RPCs are the adapter's, the Cloud
/// row's and a sandbox host's agents are OMP's (answered here), a Cloud
/// chat's `createChat` goes to its sandbox, and a forwarded call the Edge
/// would refuse never leaves.
#[tokio::test]
async fn the_engine_s_cloud_rpcs_and_relays_go_through_crew_s_adapter() {
    let (origin, requests) = serve(platform((202, sandbox("starting")), "starting")).await;
    let temp = tempfile::tempdir().unwrap();
    let core = crate::EngineCore::assemble_with_profile(
        crate::EngineProfile::local(temp.path()).unwrap(),
        Arc::new(crate::registry::default_registry()),
        HarnessId::Omp,
        None,
    )
    .unwrap();
    let bearer: Arc<dyn TokenSource> = Arc::new(StaticToken("sc_rc_owner".into()));
    let hosts = SandboxHosts::new(
        ScaffoldClient::new(&origin, "project-a", bearer.clone()).unwrap(),
        EDGE,
        Arc::new(EdgeDeviceJoinGrantClient::new(&origin, bearer).unwrap()),
    )
    .unwrap();
    let cloud = Arc::new(CrewCloud::from_parts(
        Identity::Fixed(controller()),
        core.workspace.clone(),
        hosts,
        Some("harness-zgx".into()),
        Some("deployment-a".into()),
        Ledger::open(&temp.path().join("crew")).unwrap(),
    ));
    let rpc = crate::rpc::EngineRpc::clone(&core.rpc_service()).with_crew_cloud(cloud.clone());
    use zeron_rpc::RpcService as _;
    let value = |reply: Result<RpcReply, RpcError>| match reply.expect("answered") {
        RpcReply::Value(value) => value,
        RpcReply::Stream(_) => panic!("a value"),
    };
    let status = value(rpc.handle(methods::CLOUD_STATUS, json!({})).await);
    assert_eq!((status["state"].clone(), status["deviceId"].clone()), (json!("ready"), json!("cloud")));
    for target in ["cloud", DEVICE] {
        let agents = value(rpc.handle(methods::LIST_HARNESSES, json!({"targetDeviceId": target})).await);
        let ids: Vec<_> = agents.as_array().unwrap().iter().map(|agent| agent["id"].clone()).collect();
        assert_eq!(ids, [json!("omp")], "{target}");
    }
    let refused = rpc.handle(methods::LIST_MODELS, json!({"targetDeviceId": "cloud", "harness": "codex"})).await;
    assert!(refused.err().unwrap().to_string().contains("OMP only"));

    let made = value(
        rpc.handle(
            methods::MUTATE,
            json!({"op": "createChat", "chatId": SESSION, "spaceId": "space-on-this-mac", "cloud": true, "branch": "master",
                "config": {"harness": "omp", "model": "anthropic/claude-opus-5-5", "modelOptions": {}, "sandbox": "workspace-write"}}),
        )
        .await,
    );
    assert_eq!(made, json!({"ok": true, "deviceId": DEVICE, "cwd": "/workspace/ashler-platform"}));
    let row = core.workspace.chat(SESSION).unwrap().unwrap();
    assert_eq!((row.device_id.as_str(), row.space_id.as_deref()), (DEVICE, None), "on the host, with no project");
    assert_eq!(count(&requests, "POST /api/code-sandboxes HTTP"), 1);

    // Forwarded: only what the Edge relays to the host.
    for (method, params) in [
        (methods::OPEN_TERMINAL, json!({"targetDeviceId": DEVICE, "chatId": SESSION})),
        (methods::LIST_SKILLS, json!({"targetDeviceId": DEVICE, "chatId": SESSION})),
        (methods::READ_WORKSPACE_FILE, json!({"targetDeviceId": DEVICE, "chatId": SESSION, "path": "README.md"})),
    ] {
        let refused = rpc.handle(method, params).await.err().unwrap().to_string();
        assert!(refused.contains("session-scoped Scaffold host"), "{method}: {refused}");
    }
    let _ = settled(&cloud, &format!("attach:{SANDBOX}:e1")).await;
    cloud.shutdown();
    core.sessions.shutdown().await;
}
