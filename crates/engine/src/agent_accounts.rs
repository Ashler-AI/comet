//! Shared Agent Auth account pool and one-time local credential migration.
//!
//! Agent Auth is the durable authority. The local Claude Code and Codex stores
//! are inspected only to capture credentials that predate the shared pool.
//! Recovery snapshots remain under `{data_dir}/agent-accounts/` until Agent Auth
//! acknowledges the matching import. Only then is the exact captured live
//! credential removed; a changed or failed import never deletes local material.
//!
//! New-account OAuth flows also import directly into Agent Auth. They use
//! isolated temporary state and never create a steady-state local account slot.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use comet_proto::{
    AgentAccount, AgentAccountStatus, AgentAccountWarning, AgentAccountsSnapshot, AgentAuthKind,
    AgentLoginMode, AgentLoginPoll, AgentLoginStart, AgentLoginStatus, AgentUsageWindow, HarnessId,
};

use crate::repos::home_dir;
use crate::scaffold::{
    AgentAccountCredentialImport, AgentAccountOAuthCredential, RemoteAgentAccount, ScaffoldClient,
};
use crate::{EngineError, new_id, now_ms};

// Claude Code's public OAuth client (the one the CLI itself uses for the manual
// "paste the code" flow — no secret involved, PKCE carries the proof).
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_REDIRECT: &str = "https://console.anthropic.com/oauth/code/callback";
const CLAUDE_SCOPES: &str = "org:create_api_key user:profile user:inference";
const CLAUDE_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// An abandoned login flow (dialog dismissed without Cancel) is reaped past this.
const FLOW_TTL: Duration = Duration::from_secs(15 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Filesystem knobs — env-resolved in production ([`AgentAccountsConfig::detect`]),
/// explicit in tests.
#[derive(Debug, Clone)]
pub struct AgentAccountsConfig {
    /// Engine data dir; pending migration recovery snapshots live under
    /// `{data_dir}/agent-accounts/`.
    pub data_dir: PathBuf,
    /// Claude config dir (`$CLAUDE_CONFIG_DIR` or `~/.claude`) — holds `.credentials.json`.
    pub claude_config_dir: PathBuf,
    /// Claude identity file (`~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json`).
    pub claude_config_file: PathBuf,
    /// Codex home (`$CODEX_HOME` or `~/.codex`) — holds `auth.json`.
    pub codex_home: PathBuf,
}

impl AgentAccountsConfig {
    /// Production resolution: `CLAUDE_CONFIG_DIR` relocates both the Claude config
    /// json and the credentials file; `CODEX_HOME` relocates the Codex auth file.
    pub fn detect(data_dir: &Path) -> Self {
        let env_dir = |name: &str| {
            std::env::var_os(name)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        };
        let claude_dir = env_dir("CLAUDE_CONFIG_DIR");
        let claude_config_file = match &claude_dir {
            Some(dir) => dir.join(".claude.json"),
            None => home_dir().join(".claude.json"),
        };
        Self {
            data_dir: data_dir.to_path_buf(),
            claude_config_dir: claude_dir.unwrap_or_else(|| home_dir().join(".claude")),
            claude_config_file,
            codex_home: env_dir("CODEX_HOME").unwrap_or_else(|| home_dir().join(".codex")),
        }
    }

    fn claude_creds_file(&self) -> PathBuf {
        self.claude_config_dir.join(".credentials.json")
    }

    fn codex_auth_file(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }

    fn root_dir(&self) -> PathBuf {
        self.data_dir.join("agent-accounts")
    }
}

// ── slot storage ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SlotProfile {
    email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    auth_kind: AgentAuthKind,
}

/// A credential recovery snapshot retained until Agent Auth acknowledges import.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Slot {
    id: String,
    harness: HarnessId,
    /// Provider-side identity used as Agent Auth's idempotency key.
    account_key: String,
    profile: SlotProfile,
    /// Claude: `.credentials.json`/Keychain payload. Codex: `auth.json`.
    credentials: serde_json::Value,
    /// Claude identity fields captured solely for exact-match cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claude_config: Option<serde_json::Value>,
    saved_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
}

/// A live detection result (before it's persisted into a slot).
#[derive(Debug, Clone)]
struct Detected {
    account_key: String,
    profile: SlotProfile,
    /// `None` ⇒ we know a login exists but couldn't read the secret.
    credentials: Option<serde_json::Value>,
    claude_config: Option<serde_json::Value>,
}

// ── login flows ─────────────────────────────────────────────────────────────

enum LoginFlow {
    Claude {
        verifier: String,
        /// Exchanged OAuth material retained only in memory until Agent Auth
        /// acknowledges the import. A retry must not consume the code again.
        slot: Option<Box<Slot>>,
        started_at: Instant,
    },
    Codex {
        /// The `codex login` child; monitored (try_wait) + killable from cancel.
        child: Arc<Mutex<Option<tokio::process::Child>>>,
        /// Throwaway `CODEX_HOME` — the live `~/.codex` is never touched.
        home: PathBuf,
        started_at: Instant,
        output: Arc<Mutex<String>>,
        /// `Some(code)` once the child exited (`None` code = killed by signal).
        exit: Arc<Mutex<Option<Option<i32>>>>,
    },
}

impl LoginFlow {
    fn started_at(&self) -> Instant {
        match self {
            LoginFlow::Claude { started_at, .. } | LoginFlow::Codex { started_at, .. } => {
                *started_at
            }
        }
    }
}

// ── service ─────────────────────────────────────────────────────────────────

struct Inner {
    config: AgentAccountsConfig,
    http: reqwest::Client,
    flows: Mutex<HashMap<String, LoginFlow>>,
    claude_token_url: String,
    claude_profile_url: String,
    /// Present after authenticated controller startup. Every account operation
    /// requires this client because Agent Auth is the sole authority.
    remote: Mutex<Option<ScaffoldClient>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct AgentAccounts {
    inner: Arc<Inner>,
}

impl AgentAccounts {
    pub fn new(config: AgentAccountsConfig) -> Self {
        // Startup sweep: a previous process that crashed mid-login leaves
        // `.login-<uuid>` throwaway CODEX_HOME dirs — each may hold live OAuth
        // tokens — with no owner to clean them. Reclaim them at boot.
        let root = config.root_dir();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(".login-") {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            inner: Arc::new(Inner {
                config,
                http,
                claude_token_url: CLAUDE_TOKEN_URL.into(),
                claude_profile_url: CLAUDE_PROFILE_URL.into(),
                flows: Mutex::new(HashMap::new()),
                remote: Mutex::new(None),
            }),
        }
    }

    pub fn set_remote(&self, client: ScaffoldClient) {
        *lock(&self.inner.remote) = Some(client);
    }

    // ── authoritative list and local migration ─────────────────────────────

    fn remote(&self) -> Result<ScaffoldClient, EngineError> {
        lock(&self.inner.remote).clone().ok_or_else(|| {
            EngineError::Other("Agent Auth is unavailable. Sign in to Crew and retry.".into())
        })
    }

    /// List the shared Agent Auth pool and any OAuth recovery snapshots that
    /// still need an explicit one-time import.
    pub async fn list(&self) -> Result<AgentAccountsSnapshot, EngineError> {
        let remote = self.remote()?;
        let mut warnings = Vec::new();

        let (claude, claude_warning) = self.detect_claude().await;
        if let Some(message) = claude_warning {
            warnings.push(AgentAccountWarning {
                harness: HarnessId::ClaudeCode,
                message,
            });
        }
        if let Some(detected) = claude {
            self.snapshot_detected(HarnessId::ClaudeCode, &detected)?;
        }
        if let Some(detected) = self.detect_codex() {
            if detected.profile.auth_kind == AgentAuthKind::Oauth {
                self.snapshot_detected(HarnessId::Codex, &detected)?;
            } else {
                warnings.push(AgentAccountWarning {
                    harness: HarnessId::Codex,
                    message: "API key logins remain local and cannot be migrated to the shared account pool."
                        .into(),
                });
            }
        }

        let remote_accounts = remote
            .list_agent_accounts()
            .await
            .map_err(|error| EngineError::Other(format!("Agent Auth: {error}")))?;
        let mut accounts = remote_accounts
            .iter()
            .filter_map(remote_account_for_view)
            .collect::<Vec<_>>();

        for remote_account in &remote_accounts {
            if remote_account.status != AgentAccountStatus::Connected
                && let Some(harness) = harness_for_provider(&remote_account.provider)
            {
                warnings.push(AgentAccountWarning {
                    harness,
                    message: format!(
                        "{} needs attention before it can serve new turns.",
                        remote_account.email.as_deref().unwrap_or("This account")
                    ),
                });
            }
        }

        for harness in [HarnessId::ClaudeCode, HarnessId::Codex] {
            for slot in self.read_slots(harness) {
                if slot.profile.auth_kind != AgentAuthKind::Oauth {
                    warnings.push(AgentAccountWarning {
                        harness,
                        message: "API key logins cannot be migrated to the shared account pool."
                            .into(),
                    });
                    continue;
                }

                if remote_accounts
                    .iter()
                    .any(|account| remote_account_matches_slot(account, &slot))
                {
                    self.delete_slot(&slot)?;
                    continue;
                }

                accounts.push(AgentAccount {
                    id: slot.id,
                    harness,
                    email: Some(slot.profile.email),
                    plan_label: slot.profile.plan,
                    status: AgentAccountStatus::Connected,
                    usage_windows: Vec::new(),
                    display_name: slot.profile.display_name,
                    organization: slot.profile.organization,
                    auth_kind: Some(slot.profile.auth_kind),
                    migration_available: true,
                });
            }
        }

        Ok(AgentAccountsSnapshot { accounts, warnings })
    }

    /// Import one recovery snapshot. A missing snapshot is treated as already
    /// migrated so retries remain idempotent after acknowledged cleanup.
    pub async fn migrate(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        if !matches!(harness, HarnessId::ClaudeCode | HarnessId::Codex) {
            return Err(EngineError::Other(format!(
                "agent account migration is not supported for {harness:?}"
            )));
        }
        let remote = self.remote()?;
        let Some(slot) = self
            .read_slots(harness)
            .into_iter()
            .find(|slot| slot.id == account_id)
        else {
            return self.list().await;
        };

        self.import_slot(&remote, &slot).await?;
        self.remove_matching_live_credential(&slot).await?;
        self.delete_slot(&slot)?;
        self.list().await
    }

    /// Revoke an Agent Auth account.
    pub async fn revoke(&self, account_id: &str) -> Result<AgentAccountsSnapshot, EngineError> {
        if account_id.trim().is_empty() {
            return Err(EngineError::Other("Unknown shared account.".into()));
        }
        let remote = self.remote()?;
        remote
            .revoke_agent_account(account_id)
            .await
            .map_err(|error| EngineError::Other(format!("Agent Auth removal failed: {error}")))?;
        self.list().await
    }

    async fn import_slot(&self, remote: &ScaffoldClient, slot: &Slot) -> Result<(), EngineError> {
        let import = account_import_for_slot(slot)?;
        let acknowledged = remote
            .import_agent_account(&import)
            .await
            .map_err(|error| EngineError::Other(format!("Agent Auth import failed: {error}")))?;
        if harness_for_provider(&acknowledged.provider) != Some(slot.harness)
            || acknowledged.provider_account_id != slot.account_key
        {
            return Err(EngineError::Other(
                "Agent Auth acknowledged a different account; local credentials were preserved."
                    .into(),
            ));
        }
        Ok(())
    }

    fn delete_slot(&self, slot: &Slot) -> Result<(), EngineError> {
        let file = self.slots_dir(slot.harness)?.join(format!(
            "{}.json",
            slot_id_for(slot.harness, &slot.account_key)
        ));
        if file.exists() {
            std::fs::remove_file(file)?;
        }
        Ok(())
    }

    async fn remove_matching_live_credential(&self, slot: &Slot) -> Result<(), EngineError> {
        match slot.harness {
            HarnessId::Codex => {
                let Some(detected) = self.detect_codex() else {
                    return Ok(());
                };
                if !credential_matches(slot, &detected) {
                    return Ok(());
                }
                let file = self.inner.config.codex_auth_file();
                if read_json(&file).as_ref() == Some(&slot.credentials) {
                    std::fs::remove_file(file)?;
                }
            }
            HarnessId::ClaudeCode => {
                let (Some(detected), _) = self.detect_claude().await else {
                    return Ok(());
                };
                if !credential_matches(slot, &detected) {
                    return Ok(());
                }

                let config_file = &self.inner.config.claude_config_file;
                let original_config = read_json(config_file);
                if let Some(mut cleaned) = original_config.clone()
                    && let (Some(map), Some(captured)) =
                        (cleaned.as_object_mut(), slot.claude_config.as_ref())
                {
                    if map.get("oauthAccount") == captured.get("oauthAccount") {
                        map.remove("oauthAccount");
                    }
                    if map.get("userID") == captured.get("userID") {
                        map.remove("userID");
                    }
                    write_file_atomic(config_file, cleaned.to_string().as_bytes(), false)?;
                }

                let credentials_file = self.inner.config.claude_creds_file();
                let delete_result = if credentials_file.exists() {
                    if read_json(&credentials_file).as_ref() != Some(&slot.credentials) {
                        if let Some(original) = original_config.as_ref() {
                            let _ = write_file_atomic(
                                config_file,
                                original.to_string().as_bytes(),
                                false,
                            );
                        }
                        return Ok(());
                    }
                    std::fs::remove_file(credentials_file).map_err(EngineError::from)
                } else {
                    #[cfg(target_os = "macos")]
                    {
                        let (current, _) = keychain::read_credentials().await;
                        if current.as_ref() != Some(&slot.credentials) {
                            if let Some(original) = original_config.as_ref() {
                                let _ = write_file_atomic(
                                    config_file,
                                    original.to_string().as_bytes(),
                                    false,
                                );
                            }
                            return Ok(());
                        }
                        keychain::delete_credentials().await
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        Ok(())
                    }
                };
                if let Err(error) = delete_result {
                    if let Some(original) = original_config.as_ref() {
                        let _ =
                            write_file_atomic(config_file, original.to_string().as_bytes(), false);
                    }
                    return Err(error);
                }
            }
            other => {
                return Err(EngineError::Other(format!(
                    "agent account migration is not supported for {other:?}"
                )));
            }
        }
        Ok(())
    }

    pub async fn start_login(&self, harness: HarnessId) -> Result<AgentLoginStart, EngineError> {
        let _ = self.remote()?;
        self.sweep_flows();
        match harness {
            HarnessId::ClaudeCode => Ok(self.start_claude_login()),
            HarnessId::Codex => self.start_codex_login().await,
            other => Err(EngineError::Other(format!(
                "agent logins are not supported for {other:?}"
            ))),
        }
    }

    fn start_claude_login(&self) -> AgentLoginStart {
        let login_id = new_id();
        // PKCE: 32 random bytes (two v4 uuids) as the verifier, S256 challenge.
        let raw: Vec<u8> = uuid::Uuid::new_v4()
            .as_bytes()
            .iter()
            .chain(uuid::Uuid::new_v4().as_bytes())
            .copied()
            .collect();
        let verifier = BASE64_URL.encode(&raw);
        let challenge = BASE64_URL.encode(Sha256::digest(verifier.as_bytes()));
        let url = format!(
            "https://claude.ai/oauth/authorize?code=true&client_id={CLAUDE_CLIENT_ID}\
             &response_type=code&redirect_uri={}&scope={}&code_challenge={challenge}\
             &code_challenge_method=S256&state={verifier}",
            urlencode(CLAUDE_REDIRECT),
            urlencode(CLAUDE_SCOPES),
        );
        lock(&self.inner.flows).insert(
            login_id.clone(),
            LoginFlow::Claude {
                verifier,
                slot: None,
                started_at: Instant::now(),
            },
        );
        AgentLoginStart {
            login_id,
            url,
            mode: AgentLoginMode::PasteCode,
        }
    }

    async fn start_codex_login(&self) -> Result<AgentLoginStart, EngineError> {
        // At most ONE codex login flow at a time: `codex login` binds a fixed
        // loopback OAuth port, so a lingering earlier flow makes every retry exit
        // on EADDRINUSE. Starting a new flow supersedes — and reaps — any pending.
        let stale: Vec<String> = lock(&self.inner.flows)
            .iter()
            .filter(|(_, f)| matches!(f, LoginFlow::Codex { .. }))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.cancel_login(&id);
        }

        let login_id = new_id();
        // A throwaway CODEX_HOME isolates the new login completely. Agent Auth
        // receives the credential before this directory is reclaimed.
        let home = self
            .inner
            .config
            .root_dir()
            .join(format!(".login-{login_id}"));
        std::fs::create_dir_all(&home)?;
        let mut child = match tokio::process::Command::new("codex")
            .arg("login")
            .env("CODEX_HOME", &home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&home);
                return Err(EngineError::Other(
                    if err.kind() == std::io::ErrorKind::NotFound {
                        "The `codex` CLI was not found on this device — install it first.".into()
                    } else {
                        format!("Could not start codex login: {err}")
                    },
                ));
            }
        };

        // codex prints the authorize URL (to stderr as of 0.142 — scan both
        // streams) and usually opens the browser itself; grab it so the app can
        // open it too.
        let output = Arc::new(Mutex::new(String::new()));
        for pipe in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        ]
        .into_iter()
        .flatten()
        {
            let sink = output.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut pipe = pipe;
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    lock(&sink).push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            });
        }

        let child = Arc::new(Mutex::new(Some(child)));
        let exit: Arc<Mutex<Option<Option<i32>>>> = Arc::new(Mutex::new(None));
        {
            // Monitor: poll try_wait so the child is reaped without owning it —
            // the cancel path needs concurrent kill access.
            let child = child.clone();
            let exit = exit.clone();
            tokio::spawn(async move {
                loop {
                    {
                        let mut slot = lock(&child);
                        match slot.as_mut().map(|c| c.try_wait()) {
                            None => break,
                            Some(Ok(Some(status))) => {
                                *lock(&exit) = Some(status.code());
                                *slot = None;
                                break;
                            }
                            Some(Ok(None)) => {}
                            Some(Err(_)) => {
                                *lock(&exit) = Some(None);
                                *slot = None;
                                break;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            });
        }

        lock(&self.inner.flows).insert(
            login_id.clone(),
            LoginFlow::Codex {
                child,
                home,
                started_at: Instant::now(),
                output: output.clone(),
                exit: exit.clone(),
            },
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let url = loop {
            if let Some(url) = scan_openai_url(&lock(&output)) {
                break url;
            }
            if lock(&exit).is_some() || Instant::now() > deadline {
                break String::new();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        Ok(AgentLoginStart {
            login_id,
            url,
            mode: AgentLoginMode::Browser,
        })
    }

    /// Exchange the pasted `code#state` for tokens and import the account
    /// directly into Agent Auth without touching the live Claude login.
    pub async fn complete_login(
        &self,
        login_id: &str,
        code: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        let (verifier, exchanged_slot) = match lock(&self.inner.flows).get(login_id) {
            Some(LoginFlow::Claude { verifier, slot, .. }) => (verifier.clone(), slot.clone()),
            _ => {
                return Err(EngineError::Other(
                    "This sign-in attempt expired — start again.".into(),
                ));
            }
        };
        if let Some(slot) = exchanged_slot {
            let remote = self.remote()?;
            self.import_slot(&remote, &slot).await?;
            lock(&self.inner.flows).remove(login_id);
            return self.list().await;
        }
        let (auth_code, state) = match code.trim().split_once('#') {
            Some((c, s)) => (c.to_string(), s.to_string()),
            None => (code.trim().to_string(), verifier.clone()),
        };
        if auth_code.is_empty() {
            return Err(EngineError::Other(
                "That code looks empty — paste the whole code.".into(),
            ));
        }
        let token = self
            .inner
            .http
            .post(&self.inner.claude_token_url)
            .json(&serde_json::json!({
                "grant_type": "authorization_code",
                "code": auth_code,
                "state": state,
                "client_id": CLAUDE_CLIENT_ID,
                "redirect_uri": CLAUDE_REDIRECT,
                "code_verifier": verifier,
            }))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| EngineError::Other(format!("token exchange failed: {e}")))?;
        if !token.status().is_success() {
            let status = token.status();
            let body = token.text().await.unwrap_or_default();
            let excerpt: String = body.chars().take(200).collect();
            return Err(EngineError::Other(format!(
                "Anthropic rejected the code ({status}): {excerpt}"
            )));
        }
        let token: serde_json::Value = token
            .json()
            .await
            .map_err(|e| EngineError::Other(format!("token exchange returned junk: {e}")))?;

        let access_token = str_field(&token, "access_token");
        let refresh_token = str_field(&token, "refresh_token");
        let expires_in = token
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600);
        let (Some(access_token), Some(refresh_token)) = (access_token, refresh_token) else {
            return Err(EngineError::Other(
                "Anthropic returned no usable tokens — try signing in again.".into(),
            ));
        };

        // Best-effort profile fetch — fills in the plan/org the way Claude Code does.
        let profile: Option<serde_json::Value> = match self
            .inner
            .http
            .get(&self.inner.claude_profile_url)
            .bearer_auth(&access_token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => res.json().await.ok(),
            _ => None,
        };
        let empty = serde_json::json!({});
        let p_account = profile
            .as_ref()
            .and_then(|p| p.get("account"))
            .unwrap_or(&empty);
        let p_org = profile
            .as_ref()
            .and_then(|p| p.get("organization"))
            .unwrap_or(&empty);
        let t_account = token.get("account").unwrap_or(&empty);
        let t_org = token.get("organization").unwrap_or(&empty);

        let email = str_field(p_account, "email_address")
            .or_else(|| str_field(t_account, "email_address"))
            .ok_or_else(|| {
                EngineError::Other("Could not identify the signed-in account.".into())
            })?;
        let account_uuid = str_field(p_account, "uuid")
            .or_else(|| str_field(t_account, "uuid"))
            .unwrap_or_else(|| email.clone());
        let org_name = str_field(p_org, "name").or_else(|| str_field(t_org, "name"));
        let org_type = str_field(p_org, "organization_type");
        let rate_tier = str_field(p_org, "rate_limit_tier");
        let display_name =
            str_field(p_account, "display_name").or_else(|| str_field(p_account, "full_name"));
        let subscription_type = match org_type.as_deref() {
            Some("claude_max") => Some("max"),
            Some("claude_pro") => Some("pro"),
            Some("claude_team") => Some("team"),
            Some("claude_enterprise") => Some("enterprise"),
            _ => None,
        };

        let scopes: Vec<String> = str_field(&token, "scope")
            .unwrap_or_else(|| CLAUDE_SCOPES.to_string())
            .split(' ')
            .map(str::to_string)
            .collect();
        let mut oauth = serde_json::json!({
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": now_ms() + expires_in * 1000,
            "scopes": scopes,
        });
        if let (Some(sub), Some(map)) = (subscription_type, oauth.as_object_mut()) {
            map.insert("subscriptionType".into(), serde_json::json!(sub));
        }
        let mut oauth_account = serde_json::json!({
            "accountUuid": account_uuid,
            "emailAddress": email,
            "organizationUuid": str_field(p_org, "uuid").or_else(|| str_field(t_org, "uuid")),
            "organizationName": org_name,
            "displayName": display_name,
        });
        if let Some(map) = oauth_account.as_object_mut() {
            if let Some(t) = &org_type {
                map.insert("organizationType".into(), serde_json::json!(t));
            }
            if let Some(t) = &rate_tier {
                map.insert("organizationRateLimitTier".into(), serde_json::json!(t));
            }
        }

        let slot = Slot {
            id: slot_id_for(HarnessId::ClaudeCode, &account_uuid),
            harness: HarnessId::ClaudeCode,
            account_key: account_uuid,
            profile: SlotProfile {
                email,
                display_name,
                organization: org_name,
                plan: claude_plan(org_type.as_deref(), rate_tier.as_deref()),
                auth_kind: AgentAuthKind::Oauth,
            },
            credentials: serde_json::json!({ "claudeAiOauth": oauth }),
            claude_config: Some(serde_json::json!({ "oauthAccount": oauth_account })),
            saved_at: now_ms(),
            created_at: None,
        };
        {
            let mut flows = lock(&self.inner.flows);
            match flows.get_mut(login_id) {
                Some(LoginFlow::Claude {
                    slot: pending_slot, ..
                }) => *pending_slot = Some(Box::new(slot.clone())),
                _ => {
                    return Err(EngineError::Other(
                        "This sign-in attempt expired — start again.".into(),
                    ));
                }
            }
        }
        let remote = self.remote()?;
        self.import_slot(&remote, &slot).await?;
        lock(&self.inner.flows).remove(login_id);
        self.list().await
    }

    pub async fn poll_login(&self, login_id: &str) -> Result<AgentLoginPoll, EngineError> {
        self.sweep_flows();
        let (home, exit, output) = match lock(&self.inner.flows).get(login_id) {
            None => {
                return Err(EngineError::Other(
                    "This sign-in attempt expired — start again.".into(),
                ));
            }
            Some(LoginFlow::Claude { .. }) => {
                return Ok(AgentLoginPoll {
                    status: AgentLoginStatus::Pending,
                    message: None,
                });
            }
            Some(LoginFlow::Codex {
                home, exit, output, ..
            }) => (home.clone(), exit.clone(), output.clone()),
        };
        if let Some(detected) = read_json(&home.join("auth.json")).and_then(parse_codex_auth) {
            let slot = slot_from_detected(HarnessId::Codex, &detected).ok_or_else(|| {
                EngineError::Other("Codex returned no usable credentials.".into())
            })?;
            let remote = self.remote()?;
            self.import_slot(&remote, &slot).await?;
            self.cancel_login(login_id);
            return Ok(AgentLoginPoll {
                status: AgentLoginStatus::Done,
                message: None,
            });
        }
        let exited = *lock(&exit);
        if let Some(code) = exited {
            self.cancel_login(login_id);
            let message = if code == Some(0) {
                "codex login finished without credentials.".to_string()
            } else {
                lock(&output)
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("sign-in failed")
                    .to_string()
            };
            return Ok(AgentLoginPoll {
                status: AgentLoginStatus::Error,
                message: Some(message),
            });
        }
        Ok(AgentLoginPoll {
            status: AgentLoginStatus::Pending,
            message: None,
        })
    }

    /// Drop a flow: kill a pending `codex login` child (it holds the fixed
    /// loopback OAuth port) and reclaim its throwaway home dir. Idempotent.
    pub fn cancel_login(&self, login_id: &str) {
        let flow = lock(&self.inner.flows).remove(login_id);
        if let Some(LoginFlow::Codex { child, home, .. }) = flow {
            if let Some(c) = lock(&child).as_mut() {
                let _ = c.start_kill();
            }
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    /// Engine shutdown: kill any in-flight login child so an orphan `codex login`
    /// can't survive the restart and brick the next attempt.
    pub fn shutdown(&self) {
        let ids: Vec<String> = lock(&self.inner.flows).keys().cloned().collect();
        for id in ids {
            self.cancel_login(&id);
        }
    }

    /// Lazy TTL sweep (comet uses a background fiber; native reaps on the next
    /// accounts call — same bound, no standing task).
    fn sweep_flows(&self) {
        let stale: Vec<String> = lock(&self.inner.flows)
            .iter()
            .filter(|(_, f)| f.started_at().elapsed() > FLOW_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.cancel_login(&id);
        }
    }

    // ── detection ───────────────────────────────────────────────────────────

    async fn detect_claude(&self) -> (Option<Detected>, Option<String>) {
        let cfg = read_json(&self.inner.config.claude_config_file);
        let Some(oauth) = cfg.as_ref().and_then(|c| c.get("oauthAccount")).cloned() else {
            return (None, None);
        };
        let Some(email) = str_field(&oauth, "emailAddress") else {
            return (None, None);
        };
        let (credentials, warning) = self.read_claude_credentials().await;
        let user_id = cfg.as_ref().and_then(|c| c.get("userID")).cloned();
        let mut claude_config = serde_json::json!({ "oauthAccount": oauth });
        if let (Some(uid), Some(map)) = (user_id, claude_config.as_object_mut())
            && uid.is_string()
        {
            map.insert("userID".into(), uid);
        }
        (
            Some(Detected {
                account_key: str_field(&oauth, "accountUuid").unwrap_or_else(|| email.clone()),
                profile: SlotProfile {
                    email,
                    display_name: str_field(&oauth, "displayName"),
                    organization: str_field(&oauth, "organizationName"),
                    plan: claude_plan(
                        str_field(&oauth, "organizationType").as_deref(),
                        str_field(&oauth, "organizationRateLimitTier").as_deref(),
                    ),
                    auth_kind: AgentAuthKind::Oauth,
                },
                credentials,
                claude_config: Some(claude_config),
            }),
            warning,
        )
    }

    fn detect_codex(&self) -> Option<Detected> {
        read_json(&self.inner.config.codex_auth_file()).and_then(parse_codex_auth)
    }

    /// Persist an OAuth credential only as pending import recovery data.
    fn snapshot_detected(
        &self,
        harness: HarnessId,
        detected: &Detected,
    ) -> Result<(), EngineError> {
        if detected.profile.auth_kind != AgentAuthKind::Oauth {
            return Ok(());
        }
        let Some(slot) = slot_from_detected(harness, detected) else {
            return Ok(());
        };
        self.write_slot(&slot)
    }

    // ── Claude credential store (Keychain on macOS, file elsewhere) ─────────

    /// Read the live Claude credentials. `None` payload + warning ⇒ we know a
    /// login exists but couldn't read the secret (Keychain denied us).
    async fn read_claude_credentials(&self) -> (Option<serde_json::Value>, Option<String>) {
        if let Some(creds) = read_json(&self.inner.config.claude_creds_file()) {
            return (Some(creds), None);
        }
        #[cfg(target_os = "macos")]
        {
            keychain::read_credentials().await
        }
        #[cfg(not(target_os = "macos"))]
        (None, None)
    }

    // ── slot files ──────────────────────────────────────────────────────────

    fn slots_dir(&self, harness: HarnessId) -> Result<PathBuf, EngineError> {
        let dir = self.inner.config.root_dir().join(harness_slug(harness));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn read_slots(&self, harness: HarnessId) -> Vec<Slot> {
        let Ok(dir) = self.slots_dir(harness) else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut slots: Vec<Slot> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // One malformed or misplaced snapshot must not brick the page.
            if let Some(mut slot) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Slot>(&raw).ok())
            {
                if slot.harness != harness {
                    continue;
                }
                slot.id = slot_id_for(harness, &slot.account_key);
                slots.push(slot);
            }
        }
        // Old slot files are consumed in their stable creation order.
        slots.sort_by_key(|slot| slot.created_at.unwrap_or(slot.saved_at));
        slots
    }

    fn write_slot(&self, slot: &Slot) -> Result<(), EngineError> {
        let file = self.slots_dir(slot.harness)?.join(format!(
            "{}.json",
            slot_id_for(slot.harness, &slot.account_key)
        ));
        let existing: Option<Slot> = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let mut full = slot.clone();
        full.created_at = existing
            .and_then(|e| e.created_at.or(Some(e.saved_at)))
            .or(slot.created_at)
            .or(Some(slot.saved_at));
        let json = serde_json::to_string_pretty(&full)
            .map_err(|e| EngineError::Other(format!("serialize slot: {e}")))?;
        // Atomic + 0600 from birth: tokens must never be world-readable, and a
        // crash mid-write must never leave torn JSON.
        write_file_atomic(&file, json.as_bytes(), true)
    }
}

fn harness_for_provider(provider: &str) -> Option<HarnessId> {
    match provider {
        "openai" => Some(HarnessId::Codex),
        "anthropic" => Some(HarnessId::ClaudeCode),
        _ => None,
    }
}

fn remote_account_matches_slot(account: &RemoteAgentAccount, slot: &Slot) -> bool {
    harness_for_provider(&account.provider) == Some(slot.harness)
        && account.provider_account_id == slot.account_key
}

fn remote_account_for_view(account: &RemoteAgentAccount) -> Option<AgentAccount> {
    let harness = harness_for_provider(&account.provider)?;
    let usage_windows = account
        .usage_windows
        .iter()
        .map(|window| AgentUsageWindow {
            label: window.label.clone(),
            used_fraction: window.used_fraction,
            resets_at: window
                .reset_at
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc)),
        })
        .collect();
    Some(AgentAccount {
        id: account.id.clone(),
        harness,
        email: account.email.clone(),
        plan_label: account.plan.clone(),
        status: account.status,
        usage_windows,
        display_name: account.display_name.clone(),
        organization: account.organization.clone(),
        auth_kind: Some(AgentAuthKind::Oauth),
        migration_available: false,
    })
}

fn credential_matches(slot: &Slot, detected: &Detected) -> bool {
    slot.account_key == detected.account_key
        && detected.credentials.as_ref() == Some(&slot.credentials)
}
fn slot_from_detected(harness: HarnessId, detected: &Detected) -> Option<Slot> {
    Some(Slot {
        id: slot_id_for(harness, &detected.account_key),
        harness,
        account_key: detected.account_key.clone(),
        profile: detected.profile.clone(),
        credentials: detected.credentials.clone()?,
        claude_config: detected.claude_config.clone(),
        saved_at: now_ms(),
        created_at: None,
    })
}

fn account_import_for_slot(slot: &Slot) -> Result<AgentAccountCredentialImport, EngineError> {
    let (provider, capabilities, access_token, refresh_token, expires_at, scopes) =
        match slot.harness {
            HarnessId::ClaudeCode => {
                let oauth = slot.credentials.get("claudeAiOauth").ok_or_else(|| {
                    EngineError::Other("Claude OAuth credentials are incomplete.".into())
                })?;
                let access_token = str_field(oauth, "accessToken").ok_or_else(|| {
                    EngineError::Other("Claude OAuth access token is missing.".into())
                })?;
                let refresh_token = str_field(oauth, "refreshToken").ok_or_else(|| {
                    EngineError::Other("Claude OAuth refresh token is missing.".into())
                })?;
                let expires_at = oauth
                    .get("expiresAt")
                    .and_then(|value| value.as_i64())
                    .and_then(DateTime::<Utc>::from_timestamp_millis)
                    .unwrap_or_else(|| Utc::now() + chrono::Duration::hours(1))
                    .to_rfc3339();
                let scopes = oauth
                    .get("scopes")
                    .and_then(|value| value.as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                (
                    "anthropic",
                    vec!["claude-*".into()],
                    access_token,
                    refresh_token,
                    expires_at,
                    scopes,
                )
            }
            HarnessId::Codex => {
                let tokens = slot.credentials.get("tokens").ok_or_else(|| {
                    EngineError::Other("Only ChatGPT OAuth accounts can be migrated.".into())
                })?;
                let access_token = str_field(tokens, "access_token").ok_or_else(|| {
                    EngineError::Other("ChatGPT OAuth access token is missing.".into())
                })?;
                let refresh_token = str_field(tokens, "refresh_token").ok_or_else(|| {
                    EngineError::Other("ChatGPT OAuth refresh token is missing.".into())
                })?;
                let expires_at = jwt_claims(&access_token)
                    .and_then(|claims| claims.get("exp").and_then(|value| value.as_i64()))
                    .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
                    .unwrap_or_else(|| Utc::now() + chrono::Duration::hours(1))
                    .to_rfc3339();
                (
                    "openai",
                    vec!["gpt-*".into()],
                    access_token,
                    refresh_token,
                    expires_at,
                    Vec::new(),
                )
            }
            other => {
                return Err(EngineError::Other(format!(
                    "agent account migration is not supported for {other:?}"
                )));
            }
        };
    Ok(AgentAccountCredentialImport {
        provider: provider.into(),
        provider_account_id: slot.account_key.clone(),
        email: Some(slot.profile.email.clone()),
        display_name: slot.profile.display_name.clone(),
        organization: slot.profile.organization.clone(),
        plan: slot.profile.plan.clone(),
        capabilities,
        credential: AgentAccountOAuthCredential {
            access_token,
            refresh_token,
            expires_at,
            scopes,
        },
    })
}

// ── macOS Keychain ─────────────────────────────────────────────────────────
//
// Reads discover a pending migration. Deletes happen only after Agent Auth
// acknowledges import and the live credential still exactly matches its
// recovery snapshot.
#[cfg(target_os = "macos")]
mod keychain {
    use super::*;

    const EXEC_TIMEOUT: Duration = Duration::from_secs(15);

    async fn exec(args: &[&str]) -> (bool, String, String) {
        let run = tokio::process::Command::new("security")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output();
        match tokio::time::timeout(EXEC_TIMEOUT, run).await {
            Ok(Ok(out)) => (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).to_string(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            ),
            _ => (false, String::new(), "security timed out".into()),
        }
    }

    fn account() -> String {
        std::env::var("USER").unwrap_or_else(|_| "unknown".into())
    }

    pub(super) async fn read_credentials() -> (Option<serde_json::Value>, Option<String>) {
        let (probe_ok, ..) = exec(&["find-generic-password", "-s", KEYCHAIN_SERVICE]).await;
        if !probe_ok {
            return (None, None);
        }
        let (ok, stdout, _) = exec(&[
            "find-generic-password",
            "-a",
            &account(),
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
        ])
        .await;
        if !ok {
            return (
                None,
                Some(
                    "A Claude Code login exists, but macOS Keychain denied access. Approve the prompt and refresh to migrate it."
                        .into(),
                ),
            );
        }
        match serde_json::from_str(stdout.trim()) {
            Ok(credentials) => (Some(credentials), None),
            Err(_) => (
                None,
                Some("The Claude Code Keychain entry could not be parsed.".into()),
            ),
        }
    }

    pub(super) async fn delete_credentials() -> Result<(), EngineError> {
        let (ok, _, stderr) = exec(&[
            "delete-generic-password",
            "-a",
            &account(),
            "-s",
            KEYCHAIN_SERVICE,
        ])
        .await;
        if ok {
            Ok(())
        } else {
            Err(EngineError::Other(format!(
                "Keychain delete failed: {}",
                if stderr.trim().is_empty() {
                    "unknown error"
                } else {
                    stderr.trim()
                }
            )))
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn harness_slug(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "claude-code",
        HarnessId::Codex => "codex",
        HarnessId::Omp => "omp",
        HarnessId::PrimeAgent => "prime-agent",
        HarnessId::OpenCode => "opencode",
        HarnessId::Cursor => "cursor",
        HarnessId::Mock => "mock",
    }
}

fn read_json(file: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(file).ok()?;
    serde_json::from_str(&raw)
        .ok()
        .filter(serde_json::Value::is_object)
}

fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Decode a JWT payload without verifying — we only mine identity claims from a
/// token the user's own CLI already trusts.
fn jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = BASE64_URL
        .decode(payload)
        .or_else(|_| BASE64.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn slot_id_for(harness: HarnessId, account_key: &str) -> String {
    let digest = Sha256::digest(format!("{}:{account_key}", harness_slug(harness)).as_bytes());
    crate::repos::hex(&digest)[..16].to_string()
}

/// Pretty plan label from Claude's org type + rate-limit tier ("Max 20×").
fn claude_plan(org_type: Option<&str>, tier: Option<&str>) -> Option<String> {
    let base = match org_type {
        Some("claude_max") => "Max",
        Some("claude_pro") => "Pro",
        Some("claude_team") => "Team",
        Some("claude_enterprise") => "Enterprise",
        _ => return None,
    };
    // "…_20x" style tiers carry a multiplier suffix.
    let mult = tier.and_then(|t| {
        let stem = t.strip_suffix('x')?;
        let digits: String = stem
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let preceded = stem.len() > digits.len()
            && stem.as_bytes().get(stem.len() - digits.len() - 1) == Some(&b'_');
        (!digits.is_empty() && preceded).then_some(digits)
    });
    Some(match mult {
        Some(mult) => format!("{base} {mult}×"),
        None => base.to_string(),
    })
}

fn codex_plan(plan: Option<&str>) -> Option<String> {
    let plan = plan?;
    let mut chars = plan.chars();
    let first = chars.next()?;
    Some(format!(
        "ChatGPT {}{}",
        first.to_uppercase(),
        chars.as_str()
    ))
}

/// Parse a codex `auth.json` (the live one or a fresh login's).
fn parse_codex_auth(auth: serde_json::Value) -> Option<Detected> {
    if let Some(id_token) = auth
        .get("tokens")
        .and_then(|t| t.get("id_token"))
        .and_then(|v| v.as_str())
    {
        let claims = jwt_claims(id_token).unwrap_or_else(|| serde_json::json!({}));
        let oa = claims
            .get("https://api.openai.com/auth")
            .cloned()
            .unwrap_or_default();
        let email = str_field(&claims, "email")?;
        return Some(Detected {
            account_key: str_field(&oa, "chatgpt_account_id").unwrap_or_else(|| email.clone()),
            profile: SlotProfile {
                email,
                display_name: str_field(&claims, "name"),
                organization: None,
                plan: codex_plan(str_field(&oa, "chatgpt_plan_type").as_deref()),
                auth_kind: AgentAuthKind::Oauth,
            },
            credentials: Some(auth),
            claude_config: None,
        });
    }
    let api_key = str_field(&auth, "OPENAI_API_KEY")?;
    let digest = Sha256::digest(api_key.as_bytes());
    let tail: String = api_key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Some(Detected {
        account_key: format!("api-key:{}", &crate::repos::hex(&digest)[..12]),
        profile: SlotProfile {
            email: format!("API key ·…{tail}"),
            display_name: None,
            organization: None,
            plan: Some("API key".into()),
            auth_kind: AgentAuthKind::ApiKey,
        },
        credentials: Some(auth),
        claude_config: None,
    })
}

fn scan_openai_url(output: &str) -> Option<String> {
    let start = output.find("https://auth.openai.com/")?;
    let rest = &output[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Minimal percent-encoding for OAuth query params (matches `encodeURIComponent`
/// for the constant inputs used here).
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Atomic write via a same-dir temp file + rename; `secret` = 0600 from birth.
fn write_file_atomic(file: &Path, bytes: &[u8], secret: bool) -> Result<(), EngineError> {
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if secret {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = secret;
        let mut handle = options.open(&tmp)?;
        handle.write_all(bytes)?;
    }
    std::fs::rename(&tmp, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct StaticToken;

    #[async_trait]
    impl comet_rpc::TokenSource for StaticToken {
        async fn token(&self) -> Option<String> {
            Some("test-access-token".into())
        }
    }

    async fn scripted_server(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let read = stream.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    let Some(header_end) =
                        bytes.windows(4).position(|window| window == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let content_length = String::from_utf8_lossy(&bytes[..header_end])
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let reason = if status == 200 {
                    "OK"
                } else {
                    "Internal Server Error"
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (origin, task)
    }

    fn claude_test_accounts(
        config: AgentAccountsConfig,
        claude_origin: &str,
        remote: ScaffoldClient,
    ) -> AgentAccounts {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap();
        AgentAccounts {
            inner: Arc::new(Inner {
                config,
                http,
                flows: Mutex::new(HashMap::new()),
                claude_token_url: format!("{claude_origin}/token"),
                claude_profile_url: format!("{claude_origin}/profile"),
                remote: Mutex::new(Some(remote)),
            }),
        }
    }

    fn has_temporary_claude_credential(accounts: &AgentAccounts, login_id: &str) -> bool {
        matches!(
            lock(&accounts.inner.flows).get(login_id),
            Some(LoginFlow::Claude { slot: Some(_), .. })
        )
    }

    fn claude_token_response() -> String {
        serde_json::json!({
            "access_token": "claude-access",
            "refresh_token": "claude-refresh",
            "expires_in": 3600,
            "scope": "user:profile user:inference",
            "account": {
                "uuid": "account-1",
                "email_address": "person@example.com"
            }
        })
        .to_string()
    }

    fn claude_profile_response() -> String {
        serde_json::json!({
            "account": {
                "uuid": "account-1",
                "email_address": "person@example.com",
                "display_name": "Person"
            },
            "organization": {
                "uuid": "organization-1",
                "name": "Example",
                "organization_type": "claude_pro"
            }
        })
        .to_string()
    }

    fn imported_claude_account_response() -> String {
        serde_json::json!({
            "account": {
                "id": "remote-account-1",
                "provider": "anthropic",
                "providerAccountId": "account-1",
                "email": "person@example.com",
                "displayName": "Person",
                "organization": "Example",
                "plan": "Pro",
                "status": "connected",
                "usageWindows": [{
                    "modelPattern": "claude-opus-5",
                    "label": "Opus 5",
                    "usedFraction": 1,
                    "resetAt": "2026-08-13T12:00:00.000Z"
                }]
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn list_removes_an_acknowledged_recovery_snapshot_instead_of_rendering_a_duplicate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let account =
            serde_json::from_str::<serde_json::Value>(&imported_claude_account_response()).unwrap()
                ["account"]
                .clone();
        let (remote_origin, remote_requests) = scripted_server(vec![(
            200,
            serde_json::json!({ "accounts": [account] }).to_string(),
        )])
        .await;
        let remote =
            ScaffoldClient::new(&remote_origin, "project-1", Arc::new(StaticToken)).unwrap();
        let accounts = AgentAccounts::new(config);
        accounts.set_remote(remote);
        let slot = Slot {
            id: "ignored-local-id".into(),
            harness: HarnessId::ClaudeCode,
            account_key: "account-1".into(),
            profile: SlotProfile {
                email: "person@example.com".into(),
                display_name: Some("Person".into()),
                organization: Some("Example".into()),
                plan: Some("Pro".into()),
                auth_kind: AgentAuthKind::Oauth,
            },
            credentials: serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "access",
                    "refreshToken": "refresh",
                    "expiresAt": 4_000_000_000_000_i64,
                }
            }),
            claude_config: None,
            saved_at: now_ms(),
            created_at: None,
        };
        accounts.write_slot(&slot).expect("write recovery snapshot");

        let snapshot = accounts.list().await.expect("list accounts");

        assert_eq!(snapshot.accounts.len(), 1);
        assert!(!snapshot.accounts[0].migration_available);
        assert!(accounts.read_slots(HarnessId::ClaudeCode).is_empty());
        assert_eq!(snapshot.accounts[0].usage_windows.len(), 1);
        assert_eq!(snapshot.accounts[0].usage_windows[0].label, "Opus 5");
        assert_eq!(snapshot.accounts[0].usage_windows[0].used_fraction, 1.0);
        assert_eq!(
            snapshot.accounts[0].usage_windows[0]
                .resets_at
                .as_ref()
                .map(DateTime::to_rfc3339)
                .as_deref(),
            Some("2026-08-13T12:00:00+00:00")
        );
        let requests = remote_requests.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /api/agent-accounts "));
    }

    fn test_config(root: &Path) -> AgentAccountsConfig {
        AgentAccountsConfig {
            data_dir: root.join("data"),
            claude_config_dir: root.join("claude"),
            claude_config_file: root.join("claude.json"),
            codex_home: root.join("codex"),
        }
    }

    fn codex_oauth(account_id: &str, access_token: &str) -> serde_json::Value {
        let header = BASE64_URL.encode(br#"{"alg":"none"}"#);
        let claims = BASE64_URL.encode(
            serde_json::json!({
                "email": "person@example.com",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": account_id,
                    "chatgpt_plan_type": "plus"
                }
            })
            .to_string(),
        );
        serde_json::json!({
            "tokens": {
                "id_token": format!("{header}.{claims}.x"),
                "access_token": access_token,
                "refresh_token": format!("refresh-{access_token}"),
                "account_id": account_id
            }
        })
    }
    #[tokio::test]
    async fn claude_import_retry_reuses_the_single_code_exchange() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude_origin, claude_requests) = scripted_server(vec![
            (200, claude_token_response()),
            (200, claude_profile_response()),
        ])
        .await;
        let remote_account = imported_claude_account_response();
        let list = serde_json::json!({
            "accounts": [serde_json::from_str::<serde_json::Value>(&remote_account)
                .unwrap()["account"].clone()]
        })
        .to_string();
        let (remote_origin, remote_requests) = scripted_server(vec![
            (
                500,
                serde_json::json!({ "message": "temporary" }).to_string(),
            ),
            (200, remote_account),
            (200, list),
        ])
        .await;
        let remote =
            ScaffoldClient::new(&remote_origin, "project-1", Arc::new(StaticToken)).unwrap();
        let accounts = claude_test_accounts(test_config(tmp.path()), &claude_origin, remote);
        let login = accounts.start_claude_login();

        let first = accounts
            .complete_login(&login.login_id, "one-time-code")
            .await;
        assert!(first.is_err(), "the first Agent Auth import must fail");
        assert!(
            has_temporary_claude_credential(&accounts, &login.login_id),
            "the exchanged credential remains only in the temporary flow"
        );

        accounts
            .complete_login(&login.login_id, "already-consumed-code")
            .await
            .expect("retry imports the exchanged credential");
        assert!(
            !has_temporary_claude_credential(&accounts, &login.login_id),
            "acknowledged import clears the temporary credential"
        );

        let claude_requests = claude_requests.await.unwrap();
        assert_eq!(
            claude_requests
                .iter()
                .filter(|request| request.starts_with("POST /token "))
                .count(),
            1
        );
        assert_eq!(
            claude_requests
                .iter()
                .filter(|request| request.starts_with("GET /profile "))
                .count(),
            1
        );
        let remote_requests = remote_requests.await.unwrap();
        assert_eq!(
            remote_requests
                .iter()
                .filter(|request| request.starts_with("POST /api/agent-accounts/import "))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn cancelling_claude_import_retry_clears_the_exchanged_credential() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude_origin, claude_requests) = scripted_server(vec![
            (200, claude_token_response()),
            (200, claude_profile_response()),
        ])
        .await;
        let (remote_origin, remote_requests) = scripted_server(vec![(
            500,
            serde_json::json!({ "message": "temporary" }).to_string(),
        )])
        .await;
        let remote =
            ScaffoldClient::new(&remote_origin, "project-1", Arc::new(StaticToken)).unwrap();
        let accounts = claude_test_accounts(test_config(tmp.path()), &claude_origin, remote);
        let login = accounts.start_claude_login();

        assert!(
            accounts
                .complete_login(&login.login_id, "one-time-code")
                .await
                .is_err()
        );
        assert!(has_temporary_claude_credential(&accounts, &login.login_id));
        accounts.cancel_login(&login.login_id);
        assert!(!has_temporary_claude_credential(&accounts, &login.login_id));
        assert!(
            accounts
                .complete_login(&login.login_id, "one-time-code")
                .await
                .is_err(),
            "a cancelled flow cannot reuse or exchange credentials"
        );

        let claude_requests = claude_requests.await.unwrap();
        assert_eq!(
            claude_requests
                .iter()
                .filter(|request| request.starts_with("POST /token "))
                .count(),
            1
        );
        let remote_requests = remote_requests.await.unwrap();
        assert_eq!(
            remote_requests
                .iter()
                .filter(|request| request.starts_with("POST /api/agent-accounts/import "))
                .count(),
            1
        );
    }

    #[test]
    fn imports_carry_provider_scoped_capabilities() {
        let codex = parse_codex_auth(codex_oauth("account-1", "access-1"))
            .and_then(|detected| slot_from_detected(HarnessId::Codex, &detected))
            .expect("codex slot");
        let codex_import = account_import_for_slot(&codex).expect("codex import");
        assert_eq!(codex_import.provider, "openai");
        assert_eq!(codex_import.capabilities, ["gpt-*"]);

        let claude = Slot {
            id: "local-claude".into(),
            harness: HarnessId::ClaudeCode,
            account_key: "account-2".into(),
            profile: SlotProfile {
                email: "person@example.com".into(),
                display_name: None,
                organization: None,
                plan: None,
                auth_kind: AgentAuthKind::Oauth,
            },
            credentials: serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "access-2",
                    "refreshToken": "refresh-2",
                    "expiresAt": 4_102_444_800_000i64,
                    "scopes": ["user:inference"]
                }
            }),
            claude_config: None,
            saved_at: now_ms(),
            created_at: None,
        };
        let claude_import = account_import_for_slot(&claude).expect("claude import");
        assert_eq!(claude_import.provider, "anthropic");
        assert_eq!(claude_import.capabilities, ["claude-*"]);
        assert_eq!(claude_import.credential.scopes, ["user:inference"]);
    }

    #[tokio::test]
    async fn acknowledged_cleanup_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        std::fs::create_dir_all(&config.codex_home).expect("codex home");
        let credential = codex_oauth("account-1", "access-1");
        std::fs::write(config.codex_auth_file(), credential.to_string()).expect("auth");
        let detected = parse_codex_auth(credential).expect("detected");
        let slot = slot_from_detected(HarnessId::Codex, &detected).expect("slot");
        let accounts = AgentAccounts::new(config.clone());
        accounts.write_slot(&slot).expect("snapshot");

        accounts
            .remove_matching_live_credential(&slot)
            .await
            .expect("first cleanup");
        accounts.delete_slot(&slot).expect("first snapshot cleanup");
        accounts
            .remove_matching_live_credential(&slot)
            .await
            .expect("repeated cleanup");
        accounts
            .delete_slot(&slot)
            .expect("repeated snapshot cleanup");

        assert!(!config.codex_auth_file().exists());
        assert!(
            !config
                .root_dir()
                .join("codex")
                .join(format!("{}.json", slot.id))
                .exists()
        );
    }

    #[tokio::test]
    async fn cleanup_preserves_a_changed_live_credential() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        std::fs::create_dir_all(&config.codex_home).expect("codex home");
        let captured = codex_oauth("account-1", "captured-access");
        let detected = parse_codex_auth(captured).expect("detected");
        let slot = slot_from_detected(HarnessId::Codex, &detected).expect("slot");
        let changed = codex_oauth("account-1", "new-live-access");
        std::fs::write(config.codex_auth_file(), changed.to_string()).expect("auth");
        let accounts = AgentAccounts::new(config.clone());

        accounts
            .remove_matching_live_credential(&slot)
            .await
            .expect("mismatch is not an error");

        let remaining = read_json(&config.codex_auth_file()).expect("live credential remains");
        assert_eq!(remaining, changed);
    }

    #[test]
    fn plan_labels() {
        assert_eq!(
            claude_plan(Some("claude_max"), Some("default_claude_max_20x")).as_deref(),
            Some("Max 20×")
        );
        assert_eq!(
            claude_plan(Some("claude_pro"), None).as_deref(),
            Some("Pro")
        );
        assert_eq!(
            claude_plan(Some("claude_team"), Some("weird")).as_deref(),
            Some("Team")
        );
        assert_eq!(claude_plan(Some("free"), None), None);
        assert_eq!(codex_plan(Some("plus")).as_deref(), Some("ChatGPT Plus"));
        assert_eq!(codex_plan(None), None);
    }

    #[test]
    fn openai_url_scan() {
        assert_eq!(
            scan_openai_url("open https://auth.openai.com/authorize?x=1 in your browser\n")
                .as_deref(),
            Some("https://auth.openai.com/authorize?x=1")
        );
        assert_eq!(scan_openai_url("nothing here"), None);
    }

    #[test]
    fn urlencode_matches_encode_uri_component() {
        assert_eq!(
            urlencode("org:create_api_key user:profile"),
            "org%3Acreate_api_key%20user%3Aprofile"
        );
        assert_eq!(urlencode("https://a/b"), "https%3A%2F%2Fa%2Fb");
    }
}
