//! Crew mode (src/crew.rs): the account's endpoint and kept session, the
//! account's calls answered with no Edge, no engine and no bearer, and
//! setup's and Settings' Crew screens on the host's `crew` flag.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use serde_json::{Value as Json, json};
use zgx_script::{ScriptHost, Value, testing::TestApp};

use super::{Account, Answers, ENV, MODE, NO_EDGE, PAGES, PROVIDER, Pages, RUNTIME, Runtime, SCAFFOLD_SESSION, TITLE, answer, endpoint_at, provider, sign_in_url_allowed};

/// Scaffold's https page opens, and a loopback test authority's plain http
/// one; no other plain http page, however it's dressed up.
#[test]
fn only_https_or_a_loopback_authoritys_sign_in_page_opens() {
    for url in [
        "https://scaffold-upstream-canary.internal.ashler.com/oauth/authorize?x=1",
        "http://127.0.0.1:41234/oauth/authorize",
        "http://localhost:9/authorize",
        "http://[::1]:8080/authorize",
    ] {
        assert!(sign_in_url_allowed(url), "{url}");
    }
    for url in [
        "http://scaffold.example.com/authorize",
        "http://localhost.example.com/authorize",
        "http://127.0.0.1@example.com/authorize",
        "http://127.0.0.2/authorize",
        "javascript:alert(1)",
        "file:///etc/passwd",
        "",
    ] {
        assert!(!sign_in_url_allowed(url), "{url}");
    }
}

fn ui_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui")
}

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
		ask("account.keeps_chats_local")
		ask("account.keep_chats_local", true)
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
    // Signed in is synced: no keeping chats on this device.
    assert_eq!(got["account.keeps_chats_local"], json!({ "ok": true, "value": false }));
    assert_eq!(got["account.keep_chats_local"], json!({ "ok": false, "word": "refused" }));
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

/// Setup on the `crew` flag: signed in, the look, the agents, done
/// (ui/crew_onboarding_order.luau's cases); and signing in as setup does
/// works in the project the host answers, as a member.
#[test]
fn setup_goes_crew_s_way() {
    let pages = tempfile::tempdir().unwrap();
    let crew = answers(Some(ada(Some("proj_1"))), Runtime::default(), Some(pages.path()));
    let mut app = TestApp::new(|runtime| {
        let host = ScriptHost::new(runtime, ui_dir().join("crew_onboarding_order.luau"));
        host.shared("crew", true);
        answer(&host, crew);
        host
    });
    app.frames(2);
    assert_eq!(app.error(), None);
    assert_eq!(app.text("#result"), "ok");
    assert!(app.host.run_command("crew_test.signed_in"));
    app.frames(10);
    assert_eq!(state(&app, "step"), Some(Value::from("appearance")));
    assert_eq!(state(&app, "path"), Some(Value::from("signed_in")));
    assert_eq!(state(&app, "org_name"), Some(Value::from("proj_1")));
    assert_eq!(state(&app, "org_done"), Some(Value::Bool(true)));
    assert_eq!(state(&app, "org_admin"), Some(Value::Bool(false)));
}

/// Without the flag, setup is Zeron's as it was (ui/onboarding_order.luau,
/// which `zgx test ui onboarding` runs too).
#[test]
fn setup_without_the_flag_is_zeron_s() {
    let mut app = TestApp::new(|runtime| ScriptHost::new(runtime, ui_dir().join("onboarding_order.luau")));
    app.frames(2);
    assert_eq!(app.error(), None);
    assert_eq!(app.text("#result"), "ok");
}

/// A fixture under ui/ mounted `w` by `h`, with the host's `crew` flag or
/// without it.
fn mount(fixture: &str, crew: bool, (w, h): (f32, f32)) -> TestApp {
    let app = TestApp::new(|runtime| {
        let host = ScriptHost::new(runtime, ui_dir().join(fixture));
        host.window_size(runtime.signal((w, h)));
        if crew {
            host.shared("crew", true);
        }
        host
    });
    app.ui.scene.borrow_mut().resize(w, h);
    app.frames(30);
    assert_eq!(app.error(), None, "{fixture} failed to load");
    app
}

/// The main window's shell (ui/shell_fixture.luau, Settings' calls answered
/// by ui/settings_stub.luau).
fn shell(crew: bool) -> TestApp {
    mount("shell_fixture.luau", crew, (1200., 900.))
}

/// The shell signed in, on Settings.
fn settings(crew: bool) -> TestApp {
    let app = shell(crew);
    assert!(app.host.run_command("settings_stub.sign_in"));
    assert!(app.host.run_command("shell.settings"));
    app.frames(20);
    app
}

/// What the design lint found as it built (nothing, for a screen that keeps
/// the design system's rules).
fn lint(app: &TestApp) -> Vec<String> {
    app.host.warnings().into_iter().filter(|w| w.contains("design lint")).collect()
}

/// Whether anything matches `target`: one node or several (`find` says
/// none for several).
fn shows(app: &mut TestApp, target: &str) -> bool {
    match app.target(target) {
        Ok(_) => true,
        Err(why) => !why.starts_with("nothing matches"),
    }
}

/// Crew's Settings: no Members, and an account page without the profile's
/// edits (whom you're signed in as and where, devices, signing out); its
/// About says which app this is.
#[test]
fn settings_have_no_members_and_no_profile_edits() {
    let mut app = settings(true);
    assert!(shows(&mut app, "#settings-nav >> role:tab \"Account\""), "{}", app.tree());
    assert!(!shows(&mut app, "#settings-nav >> \"Members\""), "no Members:\n{}", app.tree());
    app.click("#settings-nav >> role:tab \"Account\"");
    app.frames(30);
    assert_eq!(app.error(), None);
    // (No signing keys either: a Crew engine keeps no plugin-key directory.)
    for gone in ["Photo", "Name", "Title", "Username", "Save changes", "Delete account", "Claim a username", "Signing keys"] {
        assert!(!shows(&mut app, &format!("#settings-page >> \"{gone}\"")), "{gone} shows:\n{}", app.tree());
    }
    for kept in ["Signed in as", "ada@example.net", "Signed in on", "Sign out"] {
        assert!(shows(&mut app, &format!("#settings-page >> \"{kept}\"")), "{kept} is missing:\n{}", app.tree());
    }
    // Signing out asks to sign out of Crew.
    app.click("#settings-page >> role:button \"Sign out\"");
    app.frames(20);
    assert!(shows(&mut app, "role:dialog \"Sign out of Crew?\""), "{}", app.tree());
    app.click("role:dialog \"Sign out of Crew?\" >> role:button \"Cancel\"");
    app.frames(20);
    // Where the session works: the project, as the engine says.
    assert!(app.tree().contains("In org_1."), "{}", app.tree());
    // Nothing the design lint finds on it.
    assert!(lint(&app).is_empty(), "{:#?}", lint(&app));
    // About is under Crew's heading, not Zeron's, and still last.
    assert!(shows(&mut app, "#settings-nav >> \"Crew\""), "{}", app.tree());
    assert!(!shows(&mut app, "#settings-nav >> \"Zeron\""), "{}", app.tree());
    app.click("#settings-nav >> role:tab \"About & updates\"");
    app.frames(30);
    assert!(shows(&mut app, "#settings-page >> \"Crew (zgx dev)\""), "{}", app.tree());
    // A canary is updated by installing a newer one: nothing to check, no
    // channel, and no Zeron release notes or website.
    assert!(shows(&mut app, "#settings-page >> \"Updated by installing a newer build\""), "{}", app.tree());
    for gone in ["Check for updates", "Get beta updates", "Release notes", "Website", "zeron.sh"] {
        assert!(!shows(&mut app, &format!("#settings-page >> \"{gone}\"")), "{gone} shows:\n{}", app.tree());
    }
    // General has no "Keep chats on this device": signed in is synced.
    app.click("#settings-nav >> role:tab \"General\"");
    app.frames(30);
    assert_eq!(app.error(), None);
    assert!(shows(&mut app, "#settings-page >> \"Hide email addresses\""), "{}", app.tree());
    assert!(!shows(&mut app, "#settings-page >> \"Keep chats on this device\""), "{}", app.tree());
    // The search finds no Members page, and no keeping chats local, though
    // it still finds General's other privacy row.
    app.type_text("#settings-nav >> #settings-search", "members");
    app.frames(10);
    assert!(!shows(&mut app, "#settings-search-results >> \"Members\""), "{}", app.tree());
    // (The field itself: with results open, the search box's middle is its list.)
    let field = "#settings-search >> role:textbox \"Search settings\"";
    app.fill(field, "hide email");
    app.frames(10);
    assert!(shows(&mut app, "#settings-search-results >> \"Hide email addresses\""), "{}", app.tree());
    app.fill(field, "keep chats");
    app.frames(10);
    assert!(!shows(&mut app, "#settings-search-results >> \"Keep chats on this device\""), "{}", app.tree());
}

/// Without the flag, Settings are Zeron's as they were.
#[test]
fn settings_without_the_flag_are_zeron_s() {
    let mut app = settings(false);
    assert!(shows(&mut app, "#settings-nav >> role:tab \"Members\""), "{}", app.tree());
    assert!(shows(&mut app, "#settings-nav >> \"Zeron\""), "{}", app.tree());
    assert!(!shows(&mut app, "#settings-nav >> \"Crew\""), "{}", app.tree());
    // Providers: an account to connect, and more agents to add.
    app.click("#settings-nav >> role:tab \"Providers\"");
    app.frames(30);
    for kept in ["Connect a Claude Code account", "More agents", "Add another agent"] {
        assert!(shows(&mut app, &format!("#settings-page >> \"{kept}\"")), "{kept} is missing:\n{}", app.tree());
    }
    assert!(!shows(&mut app, "#settings-page >> \"On Crew's shared accounts\""), "{}", app.tree());
    assert!(!shows(&mut app, "#settings-page >> \"Shared accounts\""), "{}", app.tree());
    app.click("#settings-nav >> role:tab \"Account\"");
    app.frames(30);
    for kept in ["Photo", "Name", "Username", "Save changes", "Delete account"] {
        assert!(shows(&mut app, &format!("#settings-page >> \"{kept}\"")), "{kept} is missing:\n{}", app.tree());
    }
    app.click("#settings-nav >> role:tab \"About & updates\"");
    app.frames(30);
    assert!(shows(&mut app, "#settings-page >> \"Zeron\""), "{}", app.tree());
    assert!(!app.tree().contains("Crew (zgx dev)"));
    for kept in ["Release notes", "Website"] {
        assert!(shows(&mut app, &format!("#settings-page >> \"{kept}\"")), "{kept} is missing:\n{}", app.tree());
    }
    app.type_text("#settings-nav >> #settings-search", "members");
    app.frames(10);
    assert!(shows(&mut app, "#settings-search-results >> \"Members\""), "{}", app.tree());
}

/// A chat on one of Crew's sandbox hosts follows its sandbox as a Cloud chat
/// follows its machine: one that failed says so over the composer, and why
/// (ui/crew_cloud_tray_fixture.luau). Before, a failed launch said nothing at
/// all, its message waiting a day. Zeron's build doesn't take the host for
/// one of Cloud's machines, and reads nothing.
#[test]
fn a_failed_sandbox_says_so_on_its_chat() {
    let mut app = mount("crew_cloud_tray_fixture.luau", true, (600., 300.));
    assert_eq!(app.text("#alert-title"), "Machine failed");
    assert_eq!(
        app.text("#alert-body"),
        "The sandbox didn't start: missing session operation response. Start a new chat to try again."
    );
    let mut app = mount("crew_cloud_tray_fixture.luau", false, (600., 300.));
    assert_eq!(app.text("#alert-title"), "no alert");
    assert_eq!(state(&app, "crew_cloud_tray_reads").map(|reads| json_of(&reads)), Some(json!(0)));
}

/// Crew's Settings → Providers: the agents it runs, each signed in by Crew
/// (no account of its own to connect, no key to type), then Crew's shared
/// accounts, and no more agents to add.
#[test]
fn providers_are_crew_s() {
    let mut app = settings(true);
    app.click("#settings-nav >> role:tab \"Providers\"");
    app.frames(30);
    assert_eq!(app.error(), None);
    for kept in ["Claude Code", "On Crew's shared accounts", "Shared accounts", "Connect a Claude account", "Connect a ChatGPT account"] {
        assert!(shows(&mut app, &format!("#settings-page >> \"{kept}\"")), "{kept} is missing:\n{}", app.tree());
    }
    for gone in ["Connect a Claude Code account", "More agents", "Add another agent"] {
        assert!(!shows(&mut app, &format!("#settings-page >> \"{gone}\"")), "{gone} shows:\n{}", app.tree());
    }
    // The search finds no more agents to add.
    app.type_text("#settings-nav >> #settings-search", "install");
    app.frames(10);
    assert!(!shows(&mut app, "#settings-search-results >> \"More agents\""), "{}", app.tree());
    assert!(lint(&app).is_empty(), "{:#?}", lint(&app));
}

/// What the Settings stub was asked (ui/settings_stub.luau's log).
fn stub_calls(app: &TestApp) -> Vec<String> {
    state(app, "settings_stub_calls").map(|v| strings(&json_of(&v))).unwrap_or_default()
}

/// This computer's agents as a Crew engine lists them (crates/engine
/// crew_accounts.rs): the shared accounts as Claude Code's (Claude's) and
/// Codex's (ChatGPT's), and OMP's the same again by provider. An account's
/// `error` is how it stands with Agent Auth.
fn shared_agents() -> Json {
    let agent = |id: &str, name: &str, mark: &str, accounts: Json| {
        json!({
            "id": id, "name": name, "mark": mark, "installed": true, "can_install": true, "enabled": true,
            "version": "1.0.0", "installing": false, "login": "idle", "accounts": accounts,
        })
    };
    let ada = json!({
        "id": "acc_claude", "email": "ada@claude.test", "plan": "Claude Max", "session": 0.25, "week": 0.5,
        "active": false, "switchable": false, "error": "Needs attention",
        "windows": [{ "label": "Weekly", "used": 0.5 }, { "label": "Opus weekly", "used": 0.8 }],
    });
    let mut omp = ada.clone();
    omp["provider"] = json!("anthropic");
    json!({ "state": "ready", "list": [
        agent("claude-code", "Claude Code", "claude", json!([ada])),
        agent("codex", "Codex", "openai", json!([])),
        agent("omp", "OMP", "pi", json!([omp])),
    ] })
}

/// Crew's shared accounts in Settings → Providers: Claude's and ChatGPT's, a
/// card each, every account once (OMP's are the same accounts) with its
/// plan, how it stands and its usage (a model's own window in words); none
/// to put in use; removing one asks first, and adding one is its agent's
/// sign-in. The search finds them.
#[test]
fn shared_accounts_are_listed_once_and_removed_when_asked() {
    let mut app = settings(true);
    app.click("#settings-nav >> role:tab \"Providers\"");
    app.frames(30);
    app.host.set_state("agents", crate::plugins::json_value(&shared_agents())).unwrap();
    app.frames(30);
    assert_eq!(app.error(), None);
    let tree = app.tree();
    for kept in ["Shared accounts", "Claude", "ChatGPT", "Claude Max · Needs attention · Opus weekly 80%", "Connect a ChatGPT account"] {
        assert!(shows(&mut app, &format!("#settings-page >> \"{kept}\"")), "{kept} is missing:\n{tree}");
    }
    assert_eq!(tree.matches("ada@claude.test").count(), 1, "the account once:\n{tree}");
    assert!(!shows(&mut app, "#settings-page >> \"In use\""), "{tree}");
    // Its one action, removing it: asked first, and Cancel removes nothing.
    let actions = "#settings-page >> role:button \"Account actions\"";
    let dialog = "role:dialog \"Remove this shared account?\"";
    app.click(actions);
    app.frames(20);
    assert!(!shows(&mut app, "\"Switch to this account\""), "{}", app.tree());
    app.click("\"Remove account\"");
    app.frames(20);
    assert!(shows(&mut app, dialog), "{}", app.tree());
    app.click(&format!("{dialog} >> role:button \"Cancel\""));
    app.frames(20);
    assert!(!stub_calls(&app).iter().any(|c| c.starts_with("agents.remove")), "{:?}", stub_calls(&app));
    app.click(actions);
    app.frames(20);
    app.click("\"Remove account\"");
    app.frames(20);
    app.click(&format!("{dialog} >> role:button \"Remove\""));
    app.frames(20);
    assert!(stub_calls(&app).iter().any(|c| c == "agents.remove:claude-code"), "{:?}", stub_calls(&app));
    // Another ChatGPT account: Codex's sign-in. (Its card is below the fold:
    // the page scrolled to it first.)
    let connect = "#settings-page >> \"Connect a ChatGPT account\"";
    app.scroll("#setting-shared_accounts", 400.);
    app.frames(10);
    app.click(connect);
    app.frames(20);
    assert!(stub_calls(&app).iter().any(|c| c == "agents.sign_in:codex"), "{:?}", stub_calls(&app));
    assert!(lint(&app).is_empty(), "{:#?}", lint(&app));
    app.type_text("#settings-nav >> #settings-search", "shared");
    app.frames(10);
    assert!(shows(&mut app, "#settings-search-results >> \"Shared accounts\""), "{}", app.tree());
}

/// The composer's model picker (ui/chat_fixture.luau) has no Account row or
/// usage ring of its own for an agent Crew runs: a chat's account is its
/// Account option (Automatic, or one shared account). Zeron's has both.
#[test]
fn the_picker_leaves_accounts_to_the_account_option() {
    let on_device = json!({ "": { "claude-code": [{
        "id": "a1", "email": "ada@claude.test", "plan": "Claude Max", "active": true, "can_switch": false,
        "windows": [{ "label": "5h", "used": 0.34 }],
    }] } });
    for crew in [true, false] {
        let mut app = mount("chat_fixture.luau", crew, (900., 700.));
        app.host.set_state("agents.on_device", crate::plugins::json_value(&on_device)).unwrap();
        app.frames(10);
        assert_eq!(app.find("model-picker-usage").is_some(), !crew, "crew {crew}:\n{}", app.tree());
        app.click("role:button \"Model\"");
        app.frames(30);
        assert_eq!(app.error(), None);
        assert_eq!(app.find("model-picker-account").is_some(), !crew, "crew {crew}:\n{}", app.tree());
    }
}

/// Setup's Your agents (ui/providers_fixture.luau: six agents on this
/// computer, Codex with an account of its own): a Crew build's shows the two
/// it runs, signed in by Crew, and says so.
#[test]
fn setup_s_agents_are_crew_s() {
    let mut app = mount("providers_fixture.luau", true, (680., 900.));
    for kept in ["Claude Code", "Codex"] {
        assert!(shows(&mut app, &format!("\"{kept}\"")), "{kept} is missing:\n{}", app.tree());
    }
    for gone in ["Cursor", "Grok", "Devin", "Pi", "Connect a Claude Code account", "Add account", "ada@example.com"] {
        assert!(!shows(&mut app, &format!("\"{gone}\"")), "{gone} shows:\n{}", app.tree());
    }
    // (By its whole text: the tree cuts a long one short.)
    let found = "\"Found 2 agents on this computer. They run on Crew's shared accounts.\"";
    assert!(shows(&mut app, found), "{}", app.tree());
    assert!(lint(&app).is_empty(), "{:#?}", lint(&app));
    // Without the flag, every agent found, and the way to connect one.
    let mut zeron = mount("providers_fixture.luau", false, (680., 900.));
    for kept in ["Cursor", "Connect a Claude Code account"] {
        assert!(shows(&mut zeron, &format!("\"{kept}\"")), "{kept} is missing:\n{}", zeron.tree());
    }
}

/// A Crew build launched with the development switch
/// (`COMET_INFERENCE_MODE=local`: the host's `crew_managed` off) runs agents
/// on this device's own logins: setup offers every agent and its own sign-in
/// again, as Zeron's does.
#[test]
fn the_local_switch_offers_own_logins_again() {
    let mut app = TestApp::new(|runtime| {
        let host = ScriptHost::new(runtime, ui_dir().join("providers_fixture.luau"));
        host.window_size(runtime.signal((680., 900.)));
        host.shared("crew", true);
        host.shared("crew_managed", false);
        host
    });
    app.ui.scene.borrow_mut().resize(680., 900.);
    app.frames(30);
    assert_eq!(app.error(), None);
    for kept in ["Cursor", "Connect a Claude Code account", "ada@example.com"] {
        assert!(shows(&mut app, &format!("\"{kept}\"")), "{kept} is missing:\n{}", app.tree());
    }
    assert!(!shows(&mut app, "\"On Crew's shared accounts\""), "{}", app.tree());
}

/// Setup's first screen (ui/crew_signin_fixture.luau): a Crew build's
/// welcomes to Crew with no account to create and no members' wall; its one
/// pill signs in, and the quiet action at the left is Cancel, only while
/// the browser has the sign-in.
#[test]
fn the_welcome_is_crew_s() {
    let mut app = mount("crew_signin_fixture.luau", true, (680., 500.));
    assert!(app.tree().contains("to Crew"), "{}", app.tree());
    for gone in ["Create an account", "I already have an account", "Cancel"] {
        assert!(!shows(&mut app, &format!("\"{gone}\"")), "{gone} shows:\n{}", app.tree());
    }
    assert!(app.find("wall").is_none(), "a members' wall:\n{}", app.tree());
    app.click("role:button \"Sign in\"");
    app.frames(4);
    assert_eq!(state(&app, "crew_signin_went"), Some(Value::from("existing")));
    assert!(app.host.run_command("crew_signin.waiting"));
    app.frames(20);
    app.click("\"Cancel\"");
    app.frames(4);
    assert_eq!(state(&app, "crew_signin_went"), Some(Value::from("cancel")));
    assert!(lint(&app).is_empty(), "{:#?}", lint(&app));
    // Without the flag, Zeron's: an account to create, and the wall.
    let mut zeron = mount("crew_signin_fixture.luau", false, (680., 500.));
    assert!(zeron.tree().contains("to Zeron"), "{}", zeron.tree());
    zeron.click("role:button \"Create an account\"");
    zeron.frames(4);
    assert_eq!(state(&zeron, "crew_signin_went"), Some(Value::from("create")));
    assert!(shows(&mut zeron, "\"I already have an account\""), "{}", zeron.tree());
    assert!(zeron.find("wall").is_some(), "no wall:\n{}", zeron.tree());
}

/// The sidebar asks nobody to star Zeron's repository in a Crew build.
#[test]
fn the_sidebar_has_no_star_banner() {
    assert!(shell(true).find("star-banner").is_none());
    assert!(shell(false).find("star-banner").is_some());
}

/// Crew's Cloud is its Scaffold sandboxes (crates/engine crew_cloud.rs): with
/// the engine's Cloud on and the project's repository one it can open
/// (ui/cloud_stub.luau answers as the engine does), a new chat's Work in
/// offers Cloud beside this computer, and a send makes the chat there,
/// with the composer's agent and model for its sandbox's route.
#[test]
fn the_work_in_row_offers_crew_s_cloud() {
    let mut app = mount("chat_fixture.luau", true, (960., 760.));
    assert!(app.host.run_command("go_home"));
    app.frames(30);
    // (The place panel opens from the chip; closed, its rows stay, hidden.)
    let open_devices = |app: &mut TestApp| {
        app.click("role:button \"Branch\"");
        app.frames(30);
        app.click("role:menuitem \"Device\"");
        app.frames(30);
    };
    let close = |app: &mut TestApp| {
        app.keys("Escape");
        app.keys("Escape");
        app.frames(30);
    };
    // Off (no sandbox region, or signed out): this computer alone.
    open_devices(&mut app);
    assert!(!shows(&mut app, "role:option ~\"Cloud\""), "{}", app.tree());
    close(&mut app);
    assert!(app.host.run_command("cloud_stub.offer"));
    app.frames(30);
    open_devices(&mut app);
    assert!(app.shown("role:option ~\"Cloud\""), "{}", app.tree());
    app.click("role:option ~\"Cloud\"");
    app.frames(30);
    close(&mut app);
    app.type_text("role:textbox \"Message\"", "Fix the flaky test");
    app.keys("Enter");
    app.frames(30);
    assert_eq!(app.error(), None);
    // (The fixture's `calls`: what it was asked, comma-joined.)
    let said = state(&app, "calls").map(|v| json_of(&v)).unwrap_or_default();
    let calls: Vec<&str> = said.as_str().unwrap_or_default().split(',').collect();
    assert!(calls.iter().any(|call| call.ends_with("|cloud")), "{calls:#?}");
    let request = calls.iter().find(|call| call.starts_with("cloud_request:")).unwrap_or_else(|| panic!("{calls:#?}"));
    assert_ne!(*request, "cloud_request:-", "the composer's model went with it");
}

/// Crew's Settings have no Cloud or Billing pages, nor a Cloud account to
/// pick on Providers, even with its Cloud on: its sandboxes have no sizes,
/// reach, logins or billing to set.
#[test]
fn crew_s_cloud_has_no_settings_pages() {
    let mut app = settings(true);
    assert!(app.host.run_command("cloud_stub.offer"));
    app.frames(30);
    for gone in ["Cloud", "Billing"] {
        assert!(!shows(&mut app, &format!("#settings-nav >> role:tab \"{gone}\"")), "{gone} shows:\n{}", app.tree());
    }
    let mut zeron = settings(false);
    assert!(zeron.host.run_command("cloud_stub.offer"));
    zeron.frames(30);
    assert!(shows(&mut zeron, "#settings-nav >> role:tab \"Cloud\""), "{}", zeron.tree());
}
