//! Crew mode (docs/crew/zgx-thin-port.md, slice 4): a development build of
//! this branch signs in with Scaffold through its own engine, and nothing
//! here talks to WorkOS or a Zeron Edge.
//!
//!   - The account's endpoint ([`endpoint`]): no WorkOS client, the sign-in
//!     callback on port 0 (the engine's own loopback, the OS's choice), and
//!     the Edge the engine's `crew-runtime.json` names, else the loopback
//!     port nothing serves. Upstream's staging and production tables stay in
//!     src/auth.rs for later merges; no variable reaches them from here.
//!   - The engine this app starts is given no `ZERON_EDGE_URL` or
//!     `ZERON_WORKOS_CLIENT_ID` (src/engine/manager.rs): it reads its own
//!     runtime configuration.
//!   - A session kept is upstream's `session.json` or the engine's Scaffold
//!     one beside it ([`SCAFFOLD_SESSION`]).
//!   - The Zeron account's calls (a username, a card, organizations, setup's
//!     pages, choosing an organization, the profile) are answered here
//!     ([`install`], in place of src/auth.rs's and src/settings.rs's), with
//!     no Edge call and no bearer. Any other call to the Edge is refused
//!     before a bearer would be asked for (src/auth.rs `bearer`): the engine
//!     keeps its Scaffold bearer to itself.
//!   - Scripts see `account.provider = "scaffold"`, and the host's `crew`
//!     flag (ui/account/crew.luau) picks setup's one Crew way through and
//!     Settings' account page without the profile's edits or Members.
//!
//! `crew-runtime.json` (the engine's, in its data dir) as read here:
//!   { "edgeUrl": "http://127.0.0.1:8787", "scaffoldUrl": "…",
//!     "projectScope": "…", "deploymentId": "…" }
//! Only `edgeUrl` (the account's edge, said in the log) and `projectScope`
//! (the project worked in, before the engine says which) are used.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

use serde_json::{Value as Json, json};
use zgx_script::{ScriptHost, Value};

use crate::auth::{Endpoint, text};

/// Whether this build is Crew's: every development build of this branch.
/// (The `beta` feature is upstream's packaged Zeron.app, production's only;
/// Crew's packaging is slice 7's.)
pub const MODE: bool = !cfg!(feature = "beta");

/// The account's environment, as scripts see it (`account.env`).
pub const ENV: &str = "crew";

/// Who the account signs in with (`account.provider`).
pub const PROVIDER: &str = "scaffold";

/// What the app is called: its windows' title, Settings → About's (its
/// flavour's, `zeron_cli::crew::PRODUCT`).
pub const TITLE: &str = if MODE { zeron_cli::crew::PRODUCT } else { "Zeron" };

/// The engine's Scaffold session, in its data dir beside upstream's
/// `session.json`.
pub const SCAFFOLD_SESSION: &str = "scaffold-session.json";

/// The engine's runtime configuration, in its data dir.
pub const RUNTIME: &str = "crew-runtime.json";

/// What a call that would reach the Edge answers instead.
pub const NO_EDGE: &str = "refused";

/// Setup's pages finished, in this app's data dir (upstream keeps them on
/// the account, at the Edge).
const PAGES: &str = "crew-onboarding.json";

/// The account's place in its project, as an organization's role.
const ROLE: &str = "member";

/// The account's endpoint in a Crew build.
pub fn endpoint() -> Endpoint {
    endpoint_at(engine_data_dir().as_deref())
}

/// The account's endpoint for an engine with its data in `data_dir`.
pub fn endpoint_at(data_dir: Option<&Path>) -> Endpoint {
    let runtime = data_dir.map(Runtime::read).unwrap_or_default();
    Endpoint {
        name: ENV,
        edge: runtime.edge_url.unwrap_or_else(|| zeron_cli::crew::DEFAULT_EDGE_URL.into()),
        client: String::new(),
        port: 0,
    }
}

/// The engine's data dir, as src/engine/manager.rs resolves it (none where it
/// refuses one). (The edge and client it's given don't move it.)
fn engine_data_dir() -> Option<PathBuf> {
    crate::engine::manager::Config::from_env("", "", 0).ok().map(|config| config.data_dir)
}

/// Whether the engine keeps a Scaffold session in `data_dir`.
pub fn scaffold_session_kept(data_dir: &Path) -> bool {
    data_dir.join(SCAFFOLD_SESSION).is_file()
}

/// Whether a sign-in page the engine returns may be opened: Scaffold's
/// authorize page (https), or a loopback test authority's (plain http on
/// 127.0.0.1, localhost or [::1], which the engine's Scaffold mode returns
/// for a local authority). Nothing else over plain http.
pub fn sign_in_url_allowed(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once(':').map_or(authority, |(host, _)| host);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// `account.provider`: Scaffold in a Crew build, nothing in upstream's.
pub fn provider() -> Value {
    if MODE { Value::from(PROVIDER) } else { Value::Nil }
}

/// What this app reads of the engine's runtime configuration. Missing or
/// unreadable, nothing is known: the loopback Edge, no project yet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Runtime {
    pub edge_url: Option<String>,
    pub project_scope: Option<String>,
}

impl Runtime {
    pub fn read(data_dir: &Path) -> Self {
        std::fs::read(data_dir.join(RUNTIME))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Json>(&bytes).ok())
            .map(|json| Self::of(&json))
            .unwrap_or_default()
    }

    fn of(json: &Json) -> Self {
        let text = |key: &str| json[key].as_str().map(str::trim).filter(|v| !v.is_empty());
        Self {
            edge_url: text("edgeUrl")
                .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                .map(|url| url.trim_end_matches('/').into()),
            project_scope: text("projectScope").map(Into::into),
        }
    }
}

/// The signed-in account, as the engine's `AuthStatus` says it
/// (src/auth.rs `signed_in_account`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub first_name: Option<String>,
    /// The organization the session works in: Scaffold's project scope.
    pub org: Option<String>,
}

/// The project the account works in: the one its session is scoped to, else
/// the one the engine is configured for.
fn scope(account: &Account, runtime: &Runtime) -> Option<String> {
    account.org.clone().or_else(|| runtime.project_scope.clone())
}

/// `account.orgs`: that project, as the one organization the account is a
/// member of (none while it isn't known).
pub fn orgs(scope: Option<&str>) -> Value {
    let orgs: Vec<Value> = scope
        .map(|scope| {
            Value::record([
                ("id", Value::from(scope)),
                ("organization_id", Value::from(scope)),
                ("name", Value::from(scope)),
                ("role", Value::from(ROLE)),
            ])
        })
        .into_iter()
        .collect();
    Value::List(orgs.into())
}

/// `account.choose_org(id)`: the project already worked in, with nothing
/// asked of the engine (it refuses `SelectOrg` for a Scaffold session); any
/// other is refused.
pub fn choose_org(scope: Option<&str>, id: &str) -> Result<Value, &'static str> {
    match scope {
        Some(scope) if !id.is_empty() && scope == id => Ok(Value::record([("organization_id", Value::from(id))])),
        _ => Err(NO_EDGE),
    }
}

/// `account.me`: the profile as the session has it (src/settings.rs
/// `me_value`'s fields; nothing to edit, no photo).
pub fn me(account: &Account) -> Value {
    let first = account.first_name.as_deref().map_or(Value::Nil, Value::from);
    Value::record([
        ("first_name", first.clone()),
        ("name", first),
        ("email", Value::from(account.email.as_str())),
    ])
}

/// Setup's pages each account finished on this computer:
/// `{ "<account id>": { "<page>": <ms> } }`.
struct Pages {
    path: PathBuf,
}

impl Pages {
    fn read(&self) -> serde_json::Map<String, Json> {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Json>(&bytes).ok())
            .and_then(|json| json.as_object().cloned())
            .unwrap_or_default()
    }

    /// `account`'s finished pages, in name order.
    fn finished(&self, account: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .read()
            .get(account)
            .and_then(Json::as_object)
            .map(|done| done.keys().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// `ids` finished for `account`, kept (written beside, then moved into
    /// place): every page it has finished.
    fn finish(&self, account: &str, ids: &[String]) -> std::io::Result<Vec<String>> {
        let mut all = self.read();
        let done = all.entry(account.to_owned()).or_insert_with(|| json!({}));
        if !done.is_object() {
            *done = json!({});
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        if let Some(done) = done.as_object_mut() {
            for id in ids.iter().filter(|id| !id.is_empty()) {
                done.entry(id.clone()).or_insert(json!(now));
            }
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let part = self.path.with_extension("json.part");
        std::fs::write(&part, serde_json::to_vec_pretty(&Json::Object(all))?)?;
        std::fs::rename(&part, &self.path)?;
        Ok(self.finished(account))
    }
}

fn list(ids: Vec<String>) -> Value {
    Value::List(ids.iter().map(|id| Value::from(id.as_str())).collect::<Vec<_>>().into())
}

/// Where Crew's answers come from.
struct Answers {
    /// The signed-in account (none signed out).
    account: Box<dyn Fn() -> Option<Account>>,
    /// The engine's runtime configuration.
    runtime: Box<dyn Fn() -> Runtime>,
    /// Setup's pages finished (none where this app keeps nothing).
    pages: Option<Pages>,
}

impl Answers {
    fn signed_in(&self) -> Result<Account, String> {
        (self.account)().ok_or_else(|| "signed_out".to_string())
    }

    fn scope(&self, account: &Account) -> Option<String> {
        scope(account, &(self.runtime)())
    }
}

/// The `crew` flag for `host`'s scripts and, in a Crew build, the account's
/// calls answered here. Installed after src/auth.rs's and src/settings.rs's
/// (src/main.rs), whose actions of the same names these replace.
pub fn install(host: &ScriptHost) {
    host.shared("crew", MODE);
    if !MODE {
        return;
    }
    host.shared("crew_product", TITLE);
    answer(
        host,
        Rc::new(Answers {
            account: Box::new(crate::auth::signed_in_account),
            runtime: Box::new(|| engine_data_dir().map(|dir| Runtime::read(&dir)).unwrap_or_default()),
            pages: zgx_script::app_data_dir(crate::APP_ID).map(|dir| Pages { path: dir.join(PAGES) }),
        }),
    );
}

/// The account's calls on `host`, answered from `answers`: no Edge, no
/// engine, no bearer. (A template in preview is refused first, as upstream's
/// are.)
fn answer(host: &ScriptHost, answers: Rc<Answers>) {
    type Answer = Result<Value, String>;
    let refuse = crate::plugins::preview::refuse;
    // No username, no card: a Crew account has neither.
    for name in ["account.username", "account.card"] {
        host.async_action(name, move |_| async move {
            refuse(name)?;
            Answer::Ok(Value::Nil)
        });
    }
    // What only the Zeron Edge could do.
    for name in ["account.check_username", "account.claim_username", "account.create_org", "account.invite", "account.rename", "account.set_title"] {
        host.async_action(name, move |_| async move {
            refuse(name)?;
            Answer::Err(NO_EDGE.to_string())
        });
    }
    // Keeping chats local while signed in is a WorkOS profile's choice: a
    // Crew engine opens the synced workspace whenever Scaffold has signed it
    // in (crates/engine/src/lib.rs `initial_workspace_scope`), so the file
    // the switch writes would change nothing. Settings → General hides it.
    host.async_action("account.keeps_chats_local", move |_| async move {
        refuse("account.keeps_chats_local")?;
        Answer::Ok(Value::Bool(false))
    });
    host.async_action("account.keep_chats_local", move |_| async move {
        refuse("account.keep_chats_local")?;
        Answer::Err(NO_EDGE.to_string())
    });
    let a = answers.clone();
    host.async_action("account.orgs", move |_| {
        let a = a.clone();
        async move {
            refuse("account.orgs")?;
            let account = a.signed_in()?;
            Answer::Ok(orgs(a.scope(&account).as_deref()))
        }
    });
    let a = answers.clone();
    host.async_action("account.choose_org", move |id| {
        let (a, id) = (a.clone(), text(&id));
        async move {
            refuse("account.choose_org")?;
            let account = a.signed_in()?;
            choose_org(a.scope(&account).as_deref(), &id).map_err(String::from)
        }
    });
    let a = answers.clone();
    host.async_action("account.me", move |_| {
        let a = a.clone();
        async move {
            refuse("account.me")?;
            Answer::Ok(me(&a.signed_in()?))
        }
    });
    let a = answers.clone();
    host.async_action("account.onboarding", move |_| {
        let a = a.clone();
        async move {
            refuse("account.onboarding")?;
            let account = a.signed_in()?;
            Answer::Ok(list(a.pages.as_ref().map(|pages| pages.finished(&account.id)).unwrap_or_default()))
        }
    });
    let a = answers;
    host.async_action("account.onboarded", move |ids| {
        let a = a.clone();
        let ids: Vec<String> = match &ids {
            Value::List(ids) => ids.iter().map(text).collect(),
            _ => Vec::new(),
        };
        async move {
            refuse("account.onboarded")?;
            let account = a.signed_in()?;
            let Some(pages) = &a.pages else {
                return Answer::Ok(list(ids));
            };
            pages.finish(&account.id, &ids).map(list).map_err(|error| {
                eprintln!("zeron-zgx: crew: setup's pages weren't kept: {error}");
                "failed".to_string()
            })
        }
    });
}

#[cfg(test)]
#[path = "crew_tests.rs"]
mod tests;
