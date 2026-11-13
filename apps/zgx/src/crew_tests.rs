//! Crew mode (src/crew.rs): the account's endpoint and kept session, and
//! the account's calls answered with no Edge, no engine and no bearer.

use std::{
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use serde_json::{Value as Json, json};
use zgx_script::{ScriptHost, Value, testing::TestApp};

use super::{Account, Answers, ENV, MODE, NO_EDGE, PAGES, PROVIDER, Pages, RUNTIME, Runtime, SCAFFOLD_SESSION, TITLE, answer, endpoint_at, provider};

fn ada(org: Option<&str>) -> Account {
    Account { id: "user_ada".into(), email: "ada@example.net".into(), first_name: Some("Ada".into()), org: org.map(Into::into) }
}

/// The app's answers, from what's given here rather than the engine's
/// session and data dir.
fn answers(account: Option<Account>, runtime: Runtime, pages: Option<&Path>) -> Rc<Answers> {
    Rc::new(Answers {
        account: Box::new(move || account.clone()),
        runtime: Box::new(move || runtime.clone()),
        pages: pages.map(|dir| Pages { path: dir.join(PAGES) }),
    })
}

/// A state the scripts hold, by its name (a module's own answers to its
/// name alone, when no other has it).
fn state(app: &TestApp, name: &str) -> Option<Value> {
    let all = app.host.states();
    let suffix = format!("::{name}");
    let found = all.iter().find(|(n, _)| n == name).or_else(|| {
        let mut theirs = all.iter().filter(|(n, _)| n.ends_with(&suffix));
        let one = theirs.next();
        if theirs.next().is_some() { None } else { one }
    });
    found.map(|(_, v)| v.clone())
}

fn json_of(value: &Value) -> Json {
    serde_json::from_str(&value.to_json()).unwrap()
}

/// A list the scripts kept, as strings (an empty table reads as either).
fn strings(value: &Json) -> Vec<String> {
    match value {
        Json::Array(items) => items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    }
}

/// A future run to its end on this thread (its waker unparks it).
fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(std::sync::Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let std::task::Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park_timeout(Duration::from_millis(50));
    }
}

#[test]
fn a_development_build_is_crew_s() {
    assert!(MODE);
    assert_eq!((provider(), TITLE), (Value::from(PROVIDER), "Crew (zgx dev)"));
}

/// No WorkOS client, the engine's own callback (port 0), and the Edge the
/// engine's runtime configuration names, else the loopback nothing serves.
#[test]
fn the_endpoint_is_the_engine_s_own() {
    let dir = tempfile::tempdir().unwrap();
    let loopback = zeron_cli::crew::DEFAULT_EDGE_URL;
    for at in [endpoint_at(None), endpoint_at(Some(dir.path()))] {
        assert_eq!((at.name, at.edge.as_str(), at.client.as_str(), at.port), (ENV, loopback, "", 0));
    }
    std::fs::write(
        dir.path().join(RUNTIME),
        r#"{"edgeUrl":"http://127.0.0.1:8787/","scaffoldUrl":"https://scaffold.example","projectScope":"proj_1","deploymentId":"dep_1"}"#,
    )
    .unwrap();
    let at = endpoint_at(Some(dir.path()));
    assert_eq!((at.edge.as_str(), at.client.as_str(), at.port), ("http://127.0.0.1:8787", "", 0));
    assert_eq!(Runtime::read(dir.path()).project_scope.as_deref(), Some("proj_1"));
    // Not JSON, or not an address: as if there were none.
    std::fs::write(dir.path().join(RUNTIME), "{ \"edgeUrl\": ").unwrap();
    assert_eq!(endpoint_at(Some(dir.path())).edge, loopback);
    std::fs::write(dir.path().join(RUNTIME), r#"{"edgeUrl":"file:///etc/hosts","projectScope":"  "}"#).unwrap();
    assert_eq!(Runtime::read(dir.path()), Runtime::default());
}

/// A launch with the engine's Scaffold session kept is signed in, as one
/// with upstream's `session.json` is.
#[test]
fn a_kept_session_is_upstream_s_or_the_scaffold_one() {
    let dir = tempfile::tempdir().unwrap();
    let kept = || crate::auth::session_kept(dir.path());
    assert!(!kept());
    std::fs::write(dir.path().join(SCAFFOLD_SESSION), "{}").unwrap();
    assert!(kept());
    std::fs::remove_file(dir.path().join(SCAFFOLD_SESSION)).unwrap();
    std::fs::write(dir.path().join("session.json"), "{}").unwrap();
    assert!(kept());
    std::fs::remove_file(dir.path().join("session.json")).unwrap();
    std::fs::create_dir(dir.path().join(SCAFFOLD_SESSION)).unwrap();
    assert!(!kept(), "a folder of that name is no session");
}

/// Whatever still asks the Edge (Settings' profile, members, photos) is
/// refused before a bearer is asked for: no `AccessToken` (which, with no
/// engine here, would wait for one) and no request (said `offline`, or the
/// Edge's status).
#[test]
fn an_edge_call_is_refused_before_a_bearer_is_asked_for() {
    let started = Instant::now();
    assert_eq!(block_on(crate::auth::edge("GET", "/auth/me".into(), None)), Err(NO_EDGE));
    assert_eq!(block_on(crate::auth::edge("GET", "/auth/orgs/org_1/members".into(), None)), Err(NO_EDGE));
    assert_eq!(block_on(crate::auth::edge_bytes("PUT", "/auth/me/photo".into(), vec![0; 4], "image/png")), Err(NO_EDGE));
    assert!(started.elapsed() < Duration::from_secs(2), "nothing waited: {:?}", started.elapsed());
}

/// Each of the account's calls, asked as setup and Settings ask them; what
/// each answered is kept in `crew_test_said` ({ ok, value } or { ok = false,
/// word }, by call).
const ASK: &str = r#"
local said = ui.state("crew_test_said", {})
local function ask(name: string, arg: any?)
	ui.call(name, arg):after(function(value: any)
		said:set_key(name, { ok = true, value = value })
	end, function(err: any)
		said:set_key(name, { ok = false, word = err.message })
	end)
end
ui.command {
	id = "ask",
	title = "Ask",
	run = function()
		said:set({})
		ask("account.username")
		ask("account.card", "ada")
		ask("account.orgs")
		ask("account.me")
		ask("account.onboarding")
		for _, name in { "account.check_username", "account.claim_username", "account.create_org", "account.invite", "account.rename", "account.set_title" } do
			ask(name, "x")
		end
	end,
}
ui.command { id = "choose", title = "Choose", run = function() ask("account.choose_org", "proj_1") end }
ui.command { id = "choose_other", title = "Choose another", run = function() ask("account.choose_org", "org_other") end }
ui.command { id = "finish", title = "Finish", run = function() ask("account.onboarded", { "github", "card" }) end }
return ui.text { "crew" }
"#;

fn asking(answers: Rc<Answers>) -> TestApp {
    let app = TestApp::new(|runtime| ScriptHost::inline(runtime, "app", ASK));
    answer(&app.host, answers);
    app.frames(2);
    app
}

/// `command` run, and what the calls it made answered (settled in a few
/// frames: nothing they ask waits on an engine or the network).
fn said(app: &TestApp, command: &str) -> Json {
    assert!(app.host.run_command(command), "{command}");
    app.frames(4);
    state(app, "crew_test_said").map(|v| json_of(&v)).unwrap_or(Json::Null)
}

#[test]
fn the_account_s_calls_are_answered_here() {
    let pages = tempfile::tempdir().unwrap();
    let app = asking(answers(Some(ada(Some("proj_1"))), Runtime::default(), Some(pages.path())));
    let got = said(&app, "ask");
    // No username and no card: a Crew account has neither.
    assert_eq!(got["account.username"], json!({ "ok": true }));
    assert_eq!(got["account.card"], json!({ "ok": true }));
    // Its project, as the one organization it's a member of.
    assert_eq!(
        got["account.orgs"],
        json!({ "ok": true, "value": [{ "id": "proj_1", "organization_id": "proj_1", "name": "proj_1", "role": "member" }] })
    );
    assert_eq!(got["account.me"], json!({ "ok": true, "value": { "first_name": "Ada", "name": "Ada", "email": "ada@example.net" } }));
    assert_eq!(got["account.onboarding"]["ok"], json!(true));
    assert!(strings(&got["account.onboarding"]["value"]).is_empty());
    // What only Zeron's Edge could do.
    for name in ["account.check_username", "account.claim_username", "account.create_org", "account.invite", "account.rename", "account.set_title"] {
        assert_eq!(got[name], json!({ "ok": false, "word": "refused" }), "{name}");
    }
    // The project is the one to work in, with nothing asked of the engine;
    // another isn't.
    assert_eq!(said(&app, "choose")["account.choose_org"], json!({ "ok": true, "value": { "organization_id": "proj_1" } }));
    assert_eq!(said(&app, "choose_other")["account.choose_org"], json!({ "ok": false, "word": "refused" }));
    // Setup's pages, kept on this computer for the account.
    let finished = said(&app, "finish");
    assert_eq!(strings(&finished["account.onboarded"]["value"]), ["card", "github"]);
    assert_eq!(strings(&said(&app, "ask")["account.onboarding"]["value"]), ["card", "github"]);
    let kept: Json = serde_json::from_slice(&std::fs::read(pages.path().join(PAGES)).unwrap()).unwrap();
    assert!(kept["user_ada"]["github"].is_number(), "{kept}");
}

#[test]
fn the_project_is_the_engine_s_until_the_session_says() {
    let runtime = Runtime { edge_url: None, project_scope: Some("proj_2".into()) };
    let app = asking(answers(Some(ada(None)), runtime, None));
    let orgs = &said(&app, "ask")["account.orgs"]["value"];
    assert_eq!(orgs[0]["organization_id"], "proj_2");
    assert_eq!(said(&app, "choose")["account.choose_org"]["word"], "refused");
    // Nothing known of a project: no organization.
    let app = asking(answers(Some(ada(None)), Runtime::default(), None));
    let got = said(&app, "ask");
    assert_eq!(got["account.orgs"]["ok"], true);
    assert!(strings(&got["account.orgs"]["value"]).is_empty());
    // (No place to keep setup's pages: what was finished is said back.)
    assert_eq!(strings(&said(&app, "finish")["account.onboarded"]["value"]), ["github", "card"]);
}

#[test]
fn signed_out_the_account_s_calls_say_so() {
    let app = asking(answers(None, Runtime::default(), None));
    let got = said(&app, "ask");
    for name in ["account.orgs", "account.me", "account.onboarding"] {
        assert_eq!(got[name], json!({ "ok": false, "word": "signed_out" }), "{name}");
    }
    assert_eq!(said(&app, "choose")["account.choose_org"]["word"], "signed_out");
    assert_eq!(got["account.username"], json!({ "ok": true }));
}
