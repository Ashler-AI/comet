//! Crew: adding a login to Crew's shared accounts (docs/crew/zgx-thin-port.md,
//! "Agent Auth"), ported from Crew's shared sign-ins (`start_shared_login`,
//! `complete_shared_login`, `poll_shared_login`, `poll_shared_import`,
//! `cancel_shared_login` and `import_shared_slot` in
//! `crates/engine/src/agent_accounts.rs`, Ashler-AI/comet).
//!
//! A managed engine's "Add account" puts a Claude or ChatGPT login in Agent
//! Auth's shared pool, never on this device. It is upstream's capture: the
//! provider's own sign-in runs as for any account, and what it yields goes to
//! a [`CaptureSink`] instead of a slot, the CLI's store or the Keychain
//! ([`AgentAccounts::start_capture_login`]):
//!
//! - Claude: the paste-code sign-in (Anthropic's manual redirect). Its PKCE
//!   verifier and `state` are kept here, not among upstream's flows, so no
//!   upstream call can finish it into a slot.
//!   [`AgentAccounts::complete_shared_login`] redeems the code with upstream's
//!   exchange, capturing.
//! - ChatGPT: upstream's Codex capture, `codex login` into a throwaway home
//!   that is shredded before the tokens leave it, polled to completion.
//!
//! What a sign-in yields stays in memory until Agent Auth acknowledges its
//! import (`POST /api/agent-accounts/import`). An import that fails keeps it,
//! to be retried at the next poll or completion: a one-use code is redeemed
//! once. Cancelling, or [`FLOW_TTL`] passing, drops it. Nothing here writes
//! a file, a Keychain item or a CLI's credential store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde_json::Value;
use zeron_harness::crew_inference::Provider;
use zeron_proto::{AgentAuthKind, AgentLoginPoll, AgentLoginStart, AgentLoginStatus, HarnessId};

use super::{
    AgentAccounts, CLAUDE_REDIRECT, CLAUDE_SCOPES, CaptureSink, FLOW_TTL, LoginFlow, jwt_claims, lock,
    parse_codex_auth, str_field,
};
use crate::EngineError;
use crate::crew_inference::agent_auth::{AgentAccountCredential, AgentAccountImport, AgentAuthClient};

/// The shared sign-ins under way, kept by the engine's managed inference.
#[derive(Default)]
pub(crate) struct SharedLogins {
    logins: Mutex<HashMap<String, SharedLogin>>,
    /// One step of a shared sign-in at a time: a code is redeemed once, and
    /// an import never races its own retry or a cancel.
    step: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
struct SharedLogin {
    provider: Provider,
    started_at: Instant,
    /// Claude's paste-code sign-in: its PKCE verifier and `state`.
    paste: Option<(String, String)>,
    /// What the sign-in yielded (Claude's `{credential, profile}`, Codex's
    /// `auth.json`), until Agent Auth has it.
    captured: Arc<Mutex<Option<Value>>>,
}

impl SharedLogins {
    fn get(&self, login_id: &str) -> Option<SharedLogin> {
        lock(&self.logins).get(login_id).cloned()
    }

    fn remove(&self, login_id: &str) {
        lock(&self.logins).remove(login_id);
    }

    /// Sign-ins abandoned past [`FLOW_TTL`] go, their tokens with them.
    fn sweep(&self) {
        lock(&self.logins).retain(|_, login| login.started_at.elapsed() <= FLOW_TTL);
    }

    /// Every sign-in goes (the engine's runtime stops).
    pub(crate) fn clear(&self) {
        lock(&self.logins).clear();
    }
}

/// Why an import didn't land.
enum ImportFailure {
    /// It never can: the sign-in is over.
    Over(String),
    /// Agent Auth didn't take it this time: it's kept for the next try.
    NotYet(String),
}

impl ImportFailure {
    /// As a poll says it: the app goes on asking, and each poll retries.
    fn polled(&self) -> String {
        match self {
            Self::Over(why) => why.clone(),
            Self::NotYet(why) => format!(
                "The account isn't in Crew's shared accounts yet: {why}. Retrying; cancel to discard this sign-in."
            ),
        }
    }

    /// As a completion says it. (The tokens are kept all the same: a poll,
    /// or another completion, of the same sign-in imports them.)
    fn completed(&self) -> String {
        match self {
            Self::Over(why) => why.clone(),
            Self::NotYet(why) => {
                format!("The account isn't in Crew's shared accounts yet: {why}. Try again in a moment.")
            }
        }
    }
}

/// A capture that keeps what the sign-in yielded in `cell`. It never fails:
/// the import is this module's, once the capture has run.
fn keep_in(cell: Arc<Mutex<Option<Value>>>) -> CaptureSink {
    Arc::new(move |captured| {
        *lock(&cell) = Some(captured);
        Box::pin(async { Ok::<(), String>(()) })
    })
}

fn expired() -> EngineError {
    EngineError::Other("This sign-in attempt expired — start again.".into())
}

const NO_CREDENTIALS: &str = "The sign-in finished without credentials.";

fn polled(status: AgentLoginStatus, message: Option<String>) -> AgentLoginPoll {
    AgentLoginPoll {
        status,
        message,
        url: None,
        callback_port: None,
        code: None,
    }
}

impl AgentAccounts {
    /// Start adding a login of `provider`'s to the shared pool: Claude's
    /// paste-code sign-in, or ChatGPT's in the browser. The reply is
    /// upstream's own; the sign-in is driven by the calls below.
    pub(crate) async fn start_shared_login(
        &self,
        shared: &SharedLogins,
        provider: Provider,
    ) -> Result<AgentLoginStart, EngineError> {
        shared.sweep();
        let captured = Arc::new(Mutex::new(None));
        let (start, paste) = match provider {
            Provider::Anthropic => {
                self.sweep_flows();
                let start = self.start_claude_paste_login_with(None);
                // Kept here, never among upstream's flows: no upstream call
                // finishes it into a slot.
                let paste = match self.remove_flow(&start.login_id) {
                    Some(LoginFlow::Claude { verifier, state, .. }) => (verifier, state),
                    _ => return Err(EngineError::Other("Claude's sign-in didn't start. Try again.".into())),
                };
                (start, Some(paste))
            }
            Provider::Openai => {
                let start = self
                    .start_capture_login(HarnessId::Codex, keep_in(captured.clone()))
                    .await?;
                (start, None)
            }
        };
        lock(&shared.logins).insert(
            start.login_id.clone(),
            SharedLogin {
                provider,
                started_at: Instant::now(),
                paste,
                captured,
            },
        );
        Ok(start)
    }

    /// Finish a shared Claude sign-in with the code its page showed (`code`
    /// or `code#state`): redeemed once, then imported. An import Agent Auth
    /// doesn't acknowledge keeps the tokens, in memory, for the next poll or
    /// completion, which import them without redeeming anything again.
    pub(crate) async fn complete_shared_login(
        &self,
        shared: &SharedLogins,
        agent_auth: &AgentAuthClient,
        login_id: &str,
        code: &str,
    ) -> Result<(), EngineError> {
        let _step = shared.step.lock().await;
        shared.sweep();
        let login = shared.get(login_id).ok_or_else(expired)?;
        if lock(&login.captured).is_none() {
            let Some((verifier, expected_state)) = &login.paste else {
                return Err(EngineError::Other(
                    "This sign-in finishes in the browser, not with a code.".into(),
                ));
            };
            // As upstream's `complete_login` reads a pasted code.
            let (auth_code, state) = match code.trim().split_once('#') {
                Some((code, state)) => (code.to_owned(), state.to_owned()),
                None => (code.trim().to_owned(), expected_state.clone()),
            };
            if state != *expected_state {
                return Err(EngineError::Other(
                    "That code belongs to a different sign-in — start again.".into(),
                ));
            }
            if auth_code.is_empty() {
                return Err(EngineError::Other(
                    "That code looks empty — paste the whole code.".into(),
                ));
            }
            self.redeem_claude_code(
                &self.inner.endpoints.claude_token,
                &auth_code,
                &state,
                verifier,
                CLAUDE_REDIRECT,
                CLAUDE_SCOPES,
                Some(&keep_in(login.captured.clone())),
            )
            .await?;
        }
        self.import_shared(shared, agent_auth, login_id, &login)
            .await
            .map_err(|failure| EngineError::Other(failure.completed()))
    }

    /// Where a shared sign-in stands: upstream's own poll while ChatGPT's
    /// browser has it (Claude's waits for its code), then the import: done
    /// once Agent Auth acknowledges the account, pending (saying why) while
    /// it doesn't.
    pub(crate) async fn poll_shared_login(
        &self,
        shared: &SharedLogins,
        agent_auth: &AgentAuthClient,
        login_id: &str,
    ) -> Result<AgentLoginPoll, EngineError> {
        let _step = shared.step.lock().await;
        shared.sweep();
        let login = shared.get(login_id).ok_or_else(expired)?;
        if lock(&login.captured).is_none() && login.paste.is_none() {
            // ChatGPT's: upstream's capture flow, until its capture has run.
            let upstream = self.poll_login(login_id).await;
            if lock(&login.captured).is_none() {
                return match upstream {
                    Ok(poll) if poll.status == AgentLoginStatus::Pending => Ok(poll),
                    Ok(poll) => {
                        shared.remove(login_id);
                        Ok(match poll.status {
                            AgentLoginStatus::Error => poll,
                            _ => polled(AgentLoginStatus::Error, Some(NO_CREDENTIALS.into())),
                        })
                    }
                    Err(error) => {
                        shared.remove(login_id);
                        Err(error)
                    }
                };
            }
        }
        if lock(&login.captured).is_none() {
            // Claude's, waiting for its code.
            return Ok(polled(AgentLoginStatus::Pending, None));
        }
        Ok(match self.import_shared(shared, agent_auth, login_id, &login).await {
            Ok(()) => polled(AgentLoginStatus::Done, None),
            Err(failure @ ImportFailure::Over(_)) => polled(AgentLoginStatus::Error, Some(failure.polled())),
            Err(failure @ ImportFailure::NotYet(_)) => {
                polled(AgentLoginStatus::Pending, Some(failure.polled()))
            }
        })
    }

    /// Stop a sign-in (a shared one's tokens go with it). Any upstream flow
    /// of that id stops too, as upstream's cancel would.
    pub(crate) async fn cancel_shared_login(&self, shared: &SharedLogins, login_id: &str) {
        let _step = shared.step.lock().await;
        shared.remove(login_id);
        self.cancel_login(login_id);
    }

    /// Import what a shared sign-in yielded. Acknowledged, the sign-in ends;
    /// refused or unreachable, it's kept for the next try, unless what it
    /// yielded can never be imported.
    async fn import_shared(
        &self,
        shared: &SharedLogins,
        agent_auth: &AgentAuthClient,
        login_id: &str,
        login: &SharedLogin,
    ) -> Result<(), ImportFailure> {
        let captured = lock(&login.captured).clone();
        let import = match captured.as_ref().map(|captured| shared_import(login.provider, captured)) {
            Some(Ok(import)) => import,
            Some(Err(why)) => {
                self.end_shared(shared, login_id);
                return Err(ImportFailure::Over(why));
            }
            None => {
                self.end_shared(shared, login_id);
                return Err(ImportFailure::Over(NO_CREDENTIALS.into()));
            }
        };
        let account = agent_auth.import_account(&import).await.map_err(|error| {
            tracing::warn!(%error, provider = %import.provider, "crew: a shared account's import wasn't acknowledged");
            ImportFailure::NotYet(error.reason())
        })?;
        if account.provider != import.provider
            || account.provider_account_id.as_deref() != Some(import.provider_account_id.as_str())
        {
            return Err(ImportFailure::NotYet("Agent Auth answered with another account".into()));
        }
        self.end_shared(shared, login_id);
        Ok(())
    }

    fn end_shared(&self, shared: &SharedLogins, login_id: &str) {
        shared.remove(login_id);
        // (Codex's finished capture task, if upstream still lists it.)
        self.cancel_login(login_id);
    }
}

/// What Agent Auth imports from a sign-in's capture, as Crew's
/// `import_shared_slot` sends a slot: Claude's `{credential, profile}` (the
/// profile naming the account: `redeem_claude_code`), Codex's `auth.json`.
/// `Err`: it can't be imported, said for a person.
fn shared_import(provider: Provider, captured: &Value) -> Result<AgentAccountImport, String> {
    let incomplete = || "The sign-in didn't return a complete login. Start again.".to_owned();
    let required = |value: Option<String>| value.filter(|value| !value.trim().is_empty()).ok_or_else(incomplete);
    match provider {
        Provider::Anthropic => {
            let oauth = captured.get("credential").ok_or_else(incomplete)?;
            let profile = captured.get("profile").ok_or_else(incomplete)?;
            let expires_at = oauth
                .get("expiresAt")
                .and_then(Value::as_i64)
                .and_then(DateTime::<Utc>::from_timestamp_millis)
                .ok_or_else(incomplete)?;
            Ok(AgentAccountImport {
                provider: Provider::Anthropic.as_str().into(),
                provider_account_id: required(str_field(profile, "accountUuid"))?,
                email: str_field(profile, "email"),
                display_name: str_field(profile, "displayName"),
                organization: str_field(profile, "organization"),
                plan: str_field(profile, "plan"),
                capabilities: vec!["claude-*".into()],
                credential: AgentAccountCredential {
                    access_token: required(str_field(oauth, "accessToken"))?,
                    refresh_token: required(str_field(oauth, "refreshToken"))?,
                    expires_at: expires_at.to_rfc3339(),
                    scopes: oauth
                        .get("scopes")
                        .and_then(Value::as_array)
                        .map(|scopes| scopes.iter().filter_map(|scope| scope.as_str().map(str::to_owned)).collect())
                        .unwrap_or_default(),
                },
            })
        }
        Provider::Openai => {
            let detected = parse_codex_auth(captured.clone())
                .filter(|detected| detected.profile.auth_kind == AgentAuthKind::Oauth)
                .ok_or_else(incomplete)?;
            let tokens = captured.get("tokens").ok_or_else(incomplete)?;
            let access_token = required(str_field(tokens, "access_token"))?;
            let expires_at = jwt_claims(&access_token)
                .and_then(|claims| claims.get("exp").and_then(Value::as_i64))
                .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
                .ok_or_else(incomplete)?;
            Ok(AgentAccountImport {
                provider: Provider::Openai.as_str().into(),
                provider_account_id: detected.account_key,
                email: Some(detected.profile.email),
                display_name: detected.profile.display_name,
                organization: detected.profile.organization,
                plan: detected.profile.plan,
                capabilities: vec!["gpt-*".into()],
                credential: AgentAccountCredential {
                    access_token,
                    refresh_token: required(str_field(tokens, "refresh_token"))?,
                    expires_at: expires_at.to_rfc3339(),
                    scopes: Vec::new(),
                },
            })
        }
    }
}

#[cfg(test)]
impl AgentAccounts {
    /// Test seam: accounts kept under `root` (no real login, no Keychain),
    /// whose Claude sign-ins redeem at `anthropic` (`/token`, `/profile`).
    pub(crate) fn for_crew_tests(root: &std::path::Path, anthropic: &str) -> Self {
        Self::with_endpoints(
            super::AgentAccountsConfig::isolated(root),
            super::ProbeEndpoints {
                claude_token: format!("{anthropic}/token"),
                claude_profile: format!("{anthropic}/profile"),
                ..Default::default()
            },
            Default::default(),
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::{AgentAccountsConfig, ProbeEndpoints, http_response};
    use super::*;
    use std::path::{Path, PathBuf};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Every provider's store under `root`: never a real login or the Keychain.
    fn config(root: &Path) -> AgentAccountsConfig {
        AgentAccountsConfig::isolated(root)
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// Anthropic's token and profile endpoints and Agent Auth's import, on
    /// one loopback port: the token exchange redeems any code once named, the
    /// profile is unavailable (it's optional), and each import gets the next
    /// of `imports` (then 503). Anything else (another test's listener scan)
    /// is 404 and unrecorded. Records each request's head and body.
    pub(crate) async fn scripted_platform(
        imports: Vec<(&'static str, String)>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let imports = Arc::new(Mutex::new(std::collections::VecDeque::from(imports)));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..end]).to_string();
                        let length = head
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&bytes).to_string();
                let (status, body) = if request.starts_with("POST /token ") {
                    (
                        "200 OK",
                        serde_json::json!({
                            "access_token": "access", "refresh_token": "refresh", "expires_in": 3600,
                            "account": {"email_address": "new@example.com", "uuid": "acct-new"}
                        })
                        .to_string(),
                    )
                } else if request.starts_with("GET /profile ") {
                    ("503 Service Unavailable", "{}".to_owned())
                } else if request.starts_with("POST /api/agent-accounts/import ") {
                    assert!(request.contains("authorization: Bearer controller") || request.contains("Authorization: Bearer controller"));
                    lock(&imports)
                        .pop_front()
                        .unwrap_or(("503 Service Unavailable", r#"{"error":"temporarily_unavailable"}"#.into()))
                } else {
                    let _ = socket
                        .write_all(http_response("404 Not Found", &[], "").as_bytes())
                        .await;
                    continue;
                };
                lock(&seen).push(request);
                let _ = socket
                    .write_all(http_response(status, &[("Content-Type", "application/json")], &body).as_bytes())
                    .await;
                let _ = socket.shutdown().await;
            }
        });
        (origin, requests)
    }

    fn agent_auth(origin: &str) -> AgentAuthClient {
        AgentAuthClient::new(origin, Arc::new(zeron_rpc::StaticToken("controller".into()))).unwrap()
    }

    fn account(provider: &str, id: &str) -> String {
        serde_json::json!({"account": {
            "id": "shared-account", "provider": provider, "providerAccountId": id, "status": "connected"
        }})
        .to_string()
    }

    fn body_of(request: &str) -> Value {
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    /// Crew's `shared_login_retries_import_without_redeeming_or_writing_local_credentials`.
    #[tokio::test]
    async fn shared_login_retries_import_without_redeeming_or_writing_local_credentials() {
        let (origin, requests) = scripted_platform(vec![
            ("503 Service Unavailable", "{}".into()), // Agent Auth did not acknowledge.
            ("200 OK", account("anthropic", "wrong-account")),
            ("200 OK", account("anthropic", "acct-new")),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        write(&config.claude_creds_file(), "live-claude");
        write(&config.claude_config_file, "live-profile");
        write(&config.codex_auth_file(), "live-codex");
        let accounts = AgentAccounts::with_endpoints(
            config.clone(),
            ProbeEndpoints {
                claude_token: format!("{origin}/token"),
                claude_profile: format!("{origin}/profile"),
                ..Default::default()
            },
            Default::default(),
        );
        let remote = agent_auth(&origin);
        let shared = SharedLogins::default();
        let start = accounts.start_shared_login(&shared, Provider::Anthropic).await.unwrap();
        assert_eq!(start.mode, zeron_proto::AgentLoginMode::PasteCode);
        assert!(start.url.starts_with("https://claude.ai/oauth/authorize?"), "{}", start.url);
        // Upstream's own calls never finish it.
        assert!(accounts.complete_login(&start.login_id, "code").await.is_err());
        assert!(accounts.poll_login(&start.login_id).await.is_err());
        assert!(
            accounts
                .complete_shared_login(&shared, &remote, &start.login_id, "code#wrong-state")
                .await
                .is_err()
        );
        let unacknowledged = accounts
            .complete_shared_login(&shared, &remote, &start.login_id, "one-use-code")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            unacknowledged,
            "The account isn't in Crew's shared accounts yet: Agent Auth is unavailable right now \
             (agent auth request rejected). Try again in a moment."
        );
        assert!(lock(&shared.get(&start.login_id).unwrap().captured).is_some());
        let poll = accounts.poll_shared_login(&shared, &remote, &start.login_id).await.unwrap();
        assert_eq!(poll.status, AgentLoginStatus::Pending);
        assert!(poll.message.unwrap().contains("another account"));
        assert_eq!(
            accounts.poll_shared_login(&shared, &remote, &start.login_id).await.unwrap().status,
            AgentLoginStatus::Done
        );
        assert!(shared.get(&start.login_id).is_none());
        assert!(!lock(&accounts.inner.flows).contains_key(&start.login_id));
        assert!(accounts.poll_shared_login(&shared, &remote, &start.login_id).await.is_err());
        assert!(!config.root_dir().exists(), "managed OAuth must not create steady-state slots");
        assert_eq!(std::fs::read_to_string(config.claude_creds_file()).unwrap(), "live-claude");
        assert_eq!(std::fs::read_to_string(&config.claude_config_file).unwrap(), "live-profile");
        assert_eq!(std::fs::read_to_string(config.codex_auth_file()).unwrap(), "live-codex");
        let requests = lock(&requests).clone();
        assert_eq!(requests.iter().filter(|request| request.starts_with("POST /token ")).count(), 1);
        let imports: Vec<&String> =
            requests.iter().filter(|request| request.starts_with("POST /api/agent-accounts/import ")).collect();
        assert_eq!(imports.len(), 3);
        let imported = body_of(imports[2]);
        assert_eq!(imported["provider"], "anthropic");
        assert_eq!(imported["providerAccountId"], "acct-new");
        assert_eq!(imported["email"], "new@example.com");
        assert_eq!(imported["capabilities"], serde_json::json!(["claude-*"]));
        assert_eq!(imported["credential"]["accessToken"], "access");
        assert_eq!(imported["credential"]["refreshToken"], "refresh");
        assert!(DateTime::parse_from_rfc3339(imported["credential"]["expiresAt"].as_str().unwrap()).is_ok());
        assert!(!format!("{:?}", shared_import(Provider::Anthropic, &serde_json::json!({
            "credential": {"accessToken": "access-secret", "refreshToken": "refresh-secret", "expiresAt": 1},
            "profile": {"accountUuid": "a", "email": "e"},
        })).unwrap()).contains("secret"));

        let cancelled = accounts.start_shared_login(&shared, Provider::Anthropic).await.unwrap();
        accounts.cancel_shared_login(&shared, &cancelled.login_id).await;
        assert!(!lock(&accounts.inner.flows).contains_key(&cancelled.login_id));
        assert!(shared.get(&cancelled.login_id).is_none());
    }

    fn jwt(claims: Value) -> String {
        use base64::Engine as _;
        let encode = |value: &Value| super::super::BASE64_URL.encode(value.to_string());
        format!("{}.{}.signature", encode(&serde_json::json!({"alg": "none"})), encode(&claims))
    }

    /// A stand-in `codex` CLI: prints the authorize url, then writes
    /// `auth_json` into `$CODEX_HOME/auth.json` and exits.
    #[cfg(unix)]
    fn fake_codex(root: &Path, auth_json: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = root.join("fake-codex");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 echo 'Starting local login server on http://localhost:1455.' >&2\n\
                 echo 'https://auth.openai.com/oauth/authorize?response_type=code&state=s' >&2\n\
                 sleep 0.3\n\
                 printf '%s' '{auth_json}' > \"$CODEX_HOME/auth.json\"\n\
                 sleep 0.2\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// ChatGPT's sign-in is upstream's Codex capture: the throwaway home is
    /// shredded, nothing lands on this device, and an import Agent Auth
    /// refuses is retried with the same tokens at the next poll.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_shared_chatgpt_login_is_captured_and_imported_never_stored() {
        let (origin, requests) = scripted_platform(vec![
            ("503 Service Unavailable", r#"{"error":"temporarily_unavailable"}"#.into()),
            ("200 OK", account("openai", "user-1::ws-1")),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let accounts = AgentAccounts::with_endpoints(config.clone(), ProbeEndpoints::default(), Default::default());
        let expires = chrono::Utc::now().timestamp() + 3600;
        let auth = serde_json::json!({
            "tokens": {
                "id_token": jwt(serde_json::json!({
                    "email": "person@example.com", "name": "Person",
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": "ws-1", "chatgpt_user_id": "user-1", "chatgpt_plan_type": "pro"
                    }
                })),
                "access_token": jwt(serde_json::json!({ "exp": expires })),
                "refresh_token": "chatgpt-refresh",
                "account_id": "ws-1"
            }
        });
        accounts.override_cli(HarnessId::Codex, fake_codex(dir.path(), &auth.to_string()));
        let remote = agent_auth(&origin);
        let shared = SharedLogins::default();
        let start = accounts.start_shared_login(&shared, Provider::Openai).await.unwrap();
        assert_eq!(start.mode, zeron_proto::AgentLoginMode::Browser);
        assert!(start.url.starts_with("https://auth.openai.com/"), "{}", start.url);

        let mut retried = false;
        let mut settled = None;
        for _ in 0..200 {
            let poll = accounts.poll_shared_login(&shared, &remote, &start.login_id).await.unwrap();
            match poll.status {
                AgentLoginStatus::Pending if poll.message.is_some() => {
                    assert!(poll.message.unwrap().contains("unavailable right now"));
                    retried = true;
                }
                AgentLoginStatus::Pending => {}
                _ => {
                    settled = Some(poll);
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let settled = settled.expect("the sign-in settles");
        assert_eq!(settled.status, AgentLoginStatus::Done, "{:?}", settled.message);
        assert!(retried, "the first import was refused, then retried");
        assert!(shared.get(&start.login_id).is_none());
        // Nothing on this device: no throwaway home, no slot, no live login.
        let homes = std::fs::read_dir(config.root_dir())
            .map(|entries| entries.flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".login-")).count())
            .unwrap_or(0);
        assert_eq!(homes, 0);
        assert!(accounts.read_slots(HarnessId::Codex).is_empty());
        assert!(!config.codex_auth_file().exists());
        let requests = lock(&requests).clone();
        let imports: Vec<&String> =
            requests.iter().filter(|request| request.starts_with("POST /api/agent-accounts/import ")).collect();
        assert_eq!(imports.len(), 2);
        assert_eq!(body_of(imports[0]), body_of(imports[1]), "the same tokens, retried");
        let imported = body_of(imports[1]);
        assert_eq!(imported["provider"], "openai");
        assert_eq!(imported["providerAccountId"], "user-1::ws-1");
        assert_eq!(imported["email"], "person@example.com");
        assert_eq!(imported["capabilities"], serde_json::json!(["gpt-*"]));
        assert_eq!(imported["credential"]["refreshToken"], "chatgpt-refresh");
        assert_eq!(
            imported["credential"]["expiresAt"],
            DateTime::<Utc>::from_timestamp(expires, 0).unwrap().to_rfc3339()
        );
        assert!(requests.iter().all(|request| !request.starts_with("POST /token ")));
    }

    /// What can never be imported ends the sign-in, saying so; what Agent
    /// Auth answers for another account is retried.
    #[test]
    fn a_capture_is_imported_only_whole() {
        let complete = serde_json::json!({
            "credential": {"accessToken": "a", "refreshToken": "r", "expiresAt": 1_900_000_000_000_i64, "scopes": ["user:inference"]},
            "profile": {"accountUuid": "uuid-1", "email": "one@example.com", "plan": "Max"},
        });
        let import = shared_import(Provider::Anthropic, &complete).unwrap();
        assert_eq!(import.provider_account_id, "uuid-1");
        assert_eq!(import.plan.as_deref(), Some("Max"));
        assert_eq!(import.credential.scopes, ["user:inference"]);
        for broken in [
            serde_json::json!({"credential": {"accessToken": "a", "expiresAt": 1}, "profile": {"accountUuid": "u"}}),
            serde_json::json!({"credential": {"accessToken": "a", "refreshToken": "r"}, "profile": {"accountUuid": "u"}}),
            serde_json::json!({"credential": {"accessToken": "a", "refreshToken": "r", "expiresAt": 1}, "profile": {"email": "e"}}),
            serde_json::json!({"tokens": {}}),
        ] {
            assert!(shared_import(Provider::Anthropic, &broken).is_err(), "{broken}");
        }
        // An API key isn't a ChatGPT login, and a token without an expiry
        // can't be refreshed on time.
        assert!(shared_import(Provider::Openai, &serde_json::json!({"OPENAI_API_KEY": "sk-test"})).is_err());
        let no_expiry = serde_json::json!({"tokens": {
            "id_token": jwt(serde_json::json!({"email": "e@example.com"})),
            "access_token": "opaque", "refresh_token": "r"
        }});
        assert!(shared_import(Provider::Openai, &no_expiry).is_err());
    }
}
