//! Crew: a managed engine's agent accounts are Crew's shared ones
//! (docs/crew/zgx-thin-port.md, "Agent Auth"). The account RPCs answer from
//! Agent Auth (`/api/agent-accounts` on the Scaffold authority), never from
//! this device's agent logins, which managed runs don't use. Ported from
//! Crew's `crew.accounts.*` RPCs (`crates/engine/src/crew.rs`,
//! `rpc/crew.rs`) and its Settings' list (`crates/ui/src/settings/crew.rs`,
//! Ashler-AI/comet), on upstream's own methods and shapes:
//!
//! - `ListAgentAccounts` (`{forceUsage?}`): each shared login once (copies an
//!   older sign-in path saved collapse into a healthy one, as Crew's
//!   Settings show them), as rows of the agents that run on it: an Anthropic
//!   login is Claude Code's and OMP's (`provider: "anthropic"`), an OpenAI
//!   one Codex's and OMP's (`provider: "openai"`). None is active or
//!   switchable: a chat chooses its account with its Account option. A list
//!   that fails is empty, with a warning per agent saying why.
//! - `StartAgentLogin` (`{harness, provider?}`): Claude Code, or OMP with
//!   `anthropic`, adds a Claude login (paste-code); Codex, or OMP with
//!   `openai`, a ChatGPT one (browser). `PollAgentLogin`, `CompleteAgentLogin`
//!   and `CancelAgentLogin` drive it into Agent Auth (`crew_shared_login.rs`).
//!   `CompleteAgentLogin` answers the accounts, as upstream's does.
//! - `ForgetAgentAccount` (`{harness, accountId}`): revokes the account and
//!   every copy of its login, then answers the accounts.
//! - `ActivateAgentAccount`, `RenameAgentAccount`, `AddAgentApiKey`,
//!   `VaultPutApiKey` and `ConsumeCodexResetCredit` are refused.
//!
//! The shared accounts are listed a minute at a time; an import or a
//! revocation lists them again (the chats' Account option too). Only this
//! device's own IPC manages them: another device's relayed call is refused.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use zeron_harness::crew_inference::Provider;
use zeron_proto::{
    AgentAccount, AgentAccountWarning, AgentAccountsSnapshot, AgentAuthKind, AgentLoginStatus,
    AgentUsageWindow, HarnessId,
};
use zeron_rpc::{RpcError, RpcReply, methods, parse_params};

use super::agent_auth::{AgentAuthClient, AgentAuthError, RemoteAgentAccount, RemoteUsageWindow};
use super::{ACCOUNTS_FRESH, CrewInference, NOT_CONFIGURED, agent_name, lock, unsupported};
use crate::agent_accounts::AgentAccounts;

/// The agents a managed engine lists accounts for, in their order.
const AGENTS: [HarnessId; 3] = [HarnessId::ClaudeCode, HarnessId::Codex, HarnessId::Omp];

const LOCAL_ONLY: &str =
    "Crew's shared accounts are managed from this device's own Crew app, not from another device.";
const CHOSEN_PER_CHAT: &str =
    "Crew's shared accounts aren't switched for this device: each chat chooses one with its Account option.";
const NAMED_BY_LOGIN: &str = "Crew's shared accounts are named by their login, so they can't be renamed.";
const NO_API_KEYS: &str =
    "API keys aren't shared accounts: Crew runs agents on its shared Claude and ChatGPT logins.";
const NO_RESET_CREDITS: &str =
    "Rate-limit resets of Crew's shared ChatGPT accounts can't be used from this device.";
const SIGNED_OUT: &str = "Sign in to Crew to add a shared account.";

/// The account calls a managed engine answers itself.
pub(crate) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::LIST_AGENT_ACCOUNTS
            | methods::ACTIVATE_AGENT_ACCOUNT
            | methods::FORGET_AGENT_ACCOUNT
            | methods::RENAME_AGENT_ACCOUNT
            | methods::ADD_AGENT_API_KEY
            | methods::VAULT_PUT_API_KEY
            | methods::CONSUME_CODEX_RESET_CREDIT
            | methods::START_AGENT_LOGIN
            | methods::COMPLETE_AGENT_LOGIN
            | methods::POLL_AGENT_LOGIN
            | methods::CANCEL_AGENT_LOGIN
    )
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListParams {
    #[serde(default)]
    force_usage: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountParams {
    harness: HarnessId,
    account_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartParams {
    harness: HarnessId,
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginParams {
    login_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompleteParams {
    login_id: String,
    code: String,
}

fn failed(message: impl std::fmt::Display) -> RpcError {
    RpcError::Failed(message.to_string())
}

/// The shared accounts as last listed.
#[derive(Default)]
pub(super) struct Listed {
    /// When `accounts` were listed (`None`: stale, list again).
    at: Option<Instant>,
    accounts: Vec<RemoteAgentAccount>,
    /// Bumped by an import or a revocation: a list that started before it
    /// answers its caller but isn't kept as fresh.
    generation: u64,
}

impl CrewInference {
    /// A managed engine's answer to an account call (`None`: not one of
    /// them; upstream answers it).
    pub(crate) async fn accounts_rpc(
        &self,
        accounts: &AgentAccounts,
        method: &str,
        params: &serde_json::Value,
    ) -> Option<Result<RpcReply, RpcError>> {
        if !handles(method) {
            return None;
        }
        Some(self.account_call(accounts, method, params.clone()).await)
    }

    async fn account_call(
        &self,
        accounts: &AgentAccounts,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        // This device's app (or an agent's list, as upstream allows it);
        // never another device through the relay.
        if matches!(zeron_rpc::caller(), zeron_rpc::Caller::Peer { .. }) {
            return Err(failed(LOCAL_ONLY));
        }
        match method {
            methods::LIST_AGENT_ACCOUNTS => {
                let p: ListParams = serde_json::from_value(params).unwrap_or_default();
                RpcReply::value(&self.shared_accounts(p.force_usage.unwrap_or(false)).await)
            }
            methods::FORGET_AGENT_ACCOUNT => {
                let p: AccountParams = parse_params(params)?;
                RpcReply::value(&self.revoke(p.harness, &p.account_id).await.map_err(failed)?)
            }
            methods::START_AGENT_LOGIN => {
                let p: StartParams = parse_params(params)?;
                let provider = login_provider(p.harness, p.provider.as_deref()).map_err(failed)?;
                // Signed out, the login would have nowhere to go.
                if let Err(AgentAuthError::SignedOut) = self.agent_auth()?.credential().await {
                    return Err(failed(SIGNED_OUT));
                }
                let start = accounts
                    .start_shared_login(&self.shared, provider)
                    .await
                    .map_err(failed)?;
                RpcReply::value(&start)
            }
            methods::COMPLETE_AGENT_LOGIN => {
                let p: CompleteParams = parse_params(params)?;
                accounts
                    .complete_shared_login(&self.shared, self.agent_auth()?, &p.login_id, &p.code)
                    .await
                    .map_err(failed)?;
                self.accounts_changed();
                RpcReply::value(&self.shared_accounts(false).await)
            }
            methods::POLL_AGENT_LOGIN => {
                let p: LoginParams = parse_params(params)?;
                let poll = accounts
                    .poll_shared_login(&self.shared, self.agent_auth()?, &p.login_id)
                    .await
                    .map_err(failed)?;
                if poll.status == AgentLoginStatus::Done {
                    self.accounts_changed();
                }
                RpcReply::value(&poll)
            }
            methods::CANCEL_AGENT_LOGIN => {
                let p: LoginParams = parse_params(params)?;
                accounts.cancel_shared_login(&self.shared, &p.login_id).await;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::ACTIVATE_AGENT_ACCOUNT => Err(failed(CHOSEN_PER_CHAT)),
            methods::RENAME_AGENT_ACCOUNT => Err(failed(NAMED_BY_LOGIN)),
            methods::ADD_AGENT_API_KEY | methods::VAULT_PUT_API_KEY => Err(failed(NO_API_KEYS)),
            methods::CONSUME_CODEX_RESET_CREDIT => Err(failed(NO_RESET_CREDITS)),
            other => Err(RpcError::UnknownMethod(other.to_owned())),
        }
    }

    fn agent_auth(&self) -> Result<&AgentAuthClient, RpcError> {
        self.client.as_ref().ok_or_else(|| failed(NOT_CONFIGURED))
    }

    /// `ListAgentAccounts`' answer. A failure is its warnings, one per
    /// agent, never an error.
    pub(crate) async fn shared_accounts(&self, force: bool) -> AgentAccountsSnapshot {
        if self.client.is_none() {
            return unavailable(NOT_CONFIGURED);
        }
        match self.list_accounts((!force).then_some(ACCOUNTS_FRESH)).await {
            Ok(accounts) => snapshot(&accounts),
            Err(error) => {
                tracing::warn!(%error, "crew: shared accounts not listed");
                unavailable(&format!("Crew's shared accounts couldn't be loaded: {}.", error.reason()))
            }
        }
    }

    /// The shared accounts: as listed within `max_age`, else Agent Auth's
    /// now (one list at a time). No Agent Auth here: none.
    pub(super) async fn list_accounts(
        &self,
        max_age: Option<Duration>,
    ) -> Result<Vec<RemoteAgentAccount>, AgentAuthError> {
        let Some(client) = &self.client else {
            return Ok(Vec::new());
        };
        let fresh = |listed: &Listed| {
            let (max_age, at) = max_age.zip(listed.at)?;
            (at.elapsed() < max_age).then(|| listed.accounts.clone())
        };
        if let Some(accounts) = fresh(&lock(&self.listed)) {
            return Ok(accounts);
        }
        let _listing = self.listing.lock().await;
        let generation = {
            let listed = lock(&self.listed);
            if let Some(accounts) = fresh(&listed) {
                return Ok(accounts);
            }
            listed.generation
        };
        let accounts = client.list_accounts().await?;
        let mut listed = lock(&self.listed);
        if listed.generation == generation {
            *listed = Listed {
                at: Some(Instant::now()),
                accounts: accounts.clone(),
                generation,
            };
        }
        Ok(accounts)
    }

    /// The accounts as last listed, however old (none yet: none).
    pub(super) fn last_listed(&self) -> Vec<RemoteAgentAccount> {
        lock(&self.listed).accounts.clone()
    }

    /// An account was imported or revoked: list again, for Settings and the
    /// chats' Account option alike.
    fn accounts_changed(&self) {
        let mut listed = lock(&self.listed);
        listed.at = None;
        listed.generation += 1;
    }

    /// Revoke `account_id`'s login on Agent Auth, with every copy of it (as
    /// last listed, else as listed now), then answer the accounts, as
    /// upstream's removal does.
    async fn revoke(&self, harness: HarnessId, account_id: &str) -> Result<AgentAccountsSnapshot, String> {
        let agent_auth = self.client.as_ref().ok_or_else(|| NOT_CONFIGURED.to_owned())?;
        if !Provider::managed_harness(harness) {
            return Err(unsupported(harness));
        }
        let account_id = account_id.trim();
        if account_id.is_empty() {
            return Err("Choose a shared account to remove.".into());
        }
        let mut login = login_containing(&self.last_listed(), account_id);
        if login.is_none()
            && let Ok(accounts) = self.list_accounts(None).await
        {
            login = login_containing(&accounts, account_id);
        }
        let mut ids = vec![account_id.to_owned()];
        if let Some((provider, members)) = login {
            if !runs_on(harness, &provider) {
                return Err(format!("That account isn't one of {}'s.", agent_name(harness)));
            }
            ids.extend(members.into_iter().filter(|id| id != account_id));
        }
        let mut removed = 0;
        for id in &ids {
            match agent_auth.revoke_account(id).await {
                // (One Agent Auth no longer has is gone already.)
                Ok(()) => removed += 1,
                Err(error) if error.not_found() => removed += 1,
                Err(error) => {
                    tracing::warn!(%error, "crew: a shared account wasn't revoked");
                    self.accounts_changed();
                    let why = error.reason();
                    return Err(match removed {
                        0 => format!("The shared account wasn't removed: {why}."),
                        _ => format!(
                            "Removed {removed} of this account's {} saved copies; the rest weren't: {why}.",
                            ids.len()
                        ),
                    });
                }
            }
        }
        self.accounts_changed();
        Ok(self.shared_accounts(false).await)
    }
}

/// Which login a shared sign-in for `harness` adds: Claude Code's and
/// Codex's own provider's; OMP's names one (`anthropic` or `openai`).
fn login_provider(harness: HarnessId, provider: Option<&str>) -> Result<Provider, String> {
    let provider = provider.map(str::trim).filter(|provider| !provider.is_empty());
    match (harness, provider) {
        (HarnessId::ClaudeCode, None | Some("anthropic")) | (HarnessId::Omp, Some("anthropic")) => {
            Ok(Provider::Anthropic)
        }
        (HarnessId::Codex, None | Some("openai")) | (HarnessId::Omp, Some("openai")) => Ok(Provider::Openai),
        (HarnessId::Omp, None) => Err("Choose Claude or ChatGPT to add an OMP account.".into()),
        (HarnessId::ClaudeCode | HarnessId::Codex | HarnessId::Omp, Some(other)) => Err(format!(
            "{} adds {} logins to Crew's shared accounts, not {other:?}.",
            agent_name(harness),
            match harness {
                HarnessId::ClaudeCode => "Claude",
                HarnessId::Codex => "ChatGPT",
                _ => "Claude or ChatGPT",
            }
        )),
        _ => Err(format!(
            "Crew's shared accounts are for Claude Code, Codex and OMP. {} can't add one.",
            agent_name(harness)
        )),
    }
}

/// Whether `harness` runs on a login of `provider`'s.
fn runs_on(harness: HarnessId, provider: &str) -> bool {
    let provider = provider.to_ascii_lowercase();
    match harness {
        HarnessId::Omp => matches!(provider.as_str(), "anthropic" | "openai"),
        other => Provider::for_harness(other).is_some_and(|own| own.as_str() == provider),
    }
}

/// One shared login and its other saved copies (Crew's Settings'
/// `SharedLogin`). Older sign-in paths saved some ChatGPT logins twice (by
/// workspace, or by user and workspace) or under an earlier owner key. Newer
/// servers merge them; this keeps an older server's copies off the list.
pub(super) struct SharedLogin<'a> {
    pub account: &'a RemoteAgentAccount,
    pub copies: Vec<&'a RemoteAgentAccount>,
}

/// Crew's `login_key`: a ChatGPT login is one person (email) in one
/// workspace, and seats of a Team stay apart; any other login is its
/// provider's account.
fn login_key(account: &RemoteAgentAccount) -> String {
    let provider = account.provider.to_ascii_lowercase();
    let identity = account.provider_account_id.as_deref().unwrap_or(&account.id);
    match account.email.as_deref().map(str::trim).filter(|email| !email.is_empty()) {
        Some(email) if provider == "openai" => {
            let workspace = identity.split_once("::").map_or(identity, |(_, workspace)| workspace);
            format!("openai\u{1f}{workspace}\u{1f}{}", email.to_lowercase())
        }
        _ => format!("{provider}\u{1f}{identity}"),
    }
}

/// Crew's `shared_logins`: logins in Agent Auth's order, each shown through
/// a working copy when it has one (healthy first, then one keyed by user and
/// workspace). An account without an id can't be chosen or removed: skipped.
pub(super) fn shared_logins(accounts: &[RemoteAgentAccount]) -> Vec<SharedLogin<'_>> {
    let mut logins: Vec<(String, Vec<&RemoteAgentAccount>)> = Vec::new();
    for account in accounts.iter().filter(|account| !account.id.trim().is_empty()) {
        let key = login_key(account);
        match logins.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, copies)) => copies.push(account),
            None => logins.push((key, vec![account])),
        }
    }
    logins
        .into_iter()
        .map(|(_, mut copies)| {
            copies.sort_by_key(|account| {
                (
                    !account.healthy(),
                    !account.provider_account_id.as_deref().is_some_and(|id| id.contains("::")),
                )
            });
            let account = copies.remove(0);
            SharedLogin { account, copies }
        })
        .collect()
}

/// The login `account_id` is (or is a copy of): its provider and every id it
/// was saved under, the one shown first.
fn login_containing(accounts: &[RemoteAgentAccount], account_id: &str) -> Option<(String, Vec<String>)> {
    shared_logins(accounts).into_iter().find_map(|login| {
        let ids: Vec<String> = std::iter::once(login.account)
            .chain(login.copies)
            .map(|account| account.id.clone())
            .collect();
        ids.iter()
            .any(|id| id == account_id)
            .then(|| (login.account.provider.clone(), ids))
    })
}

/// The rows of a login's agents: Anthropic's is Claude Code's and OMP's,
/// OpenAI's Codex's and OMP's (OMP's grouped by provider).
fn agents_of(provider: &str) -> &'static [(HarnessId, Option<&'static str>)] {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => &[(HarnessId::ClaudeCode, None), (HarnessId::Omp, Some("anthropic"))],
        "openai" => &[(HarnessId::Codex, None), (HarnessId::Omp, Some("openai"))],
        _ => &[],
    }
}

/// `ListAgentAccounts`' rows: each agent's logins, in Agent Auth's order.
fn snapshot(accounts: &[RemoteAgentAccount]) -> AgentAccountsSnapshot {
    let logins = shared_logins(accounts);
    let mut rows = Vec::new();
    for agent in AGENTS {
        for login in &logins {
            for (harness, group) in agents_of(&login.account.provider) {
                if *harness == agent {
                    rows.push(row(login.account, agent, *group));
                }
            }
        }
    }
    AgentAccountsSnapshot {
        accounts: rows,
        warnings: Vec::new(),
    }
}

fn row(account: &RemoteAgentAccount, harness: HarnessId, provider: Option<&str>) -> AgentAccount {
    let text = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    AgentAccount {
        id: account.id.clone(),
        harness,
        email: text(&account.email),
        plan_label: account.plan_label().map(str::to_owned),
        active: false,
        usage_windows: usage_windows(&account.usage_windows),
        usage_fetched_at: None,
        // Crew's Settings' words for an account Agent Auth won't route to.
        usage_error: (!account.healthy()).then(|| {
            match account.status.as_str() {
                "disabled" => "Disabled",
                "revoked" => "Revoked",
                _ => "Needs attention",
            }
            .to_owned()
        }),
        codex_reset_credits: None,
        display_name: text(&account.display_name),
        organization: text(&account.organization),
        auth_kind: Some(AgentAuthKind::Oauth),
        switchable: false,
        saved_at: None,
        provider: provider.map(str::to_owned),
    }
}

/// Agent Auth's plan windows as Settings' meters: each with a label and a
/// usable fraction (kept within 0..=1), its reset when it names one. The
/// whole family's come first, its shorter before its weekly one, as
/// upstream reads a list (the first its session, the one saying "week" its
/// week); a model's or tier's own ("Opus weekly", "5-hour (Spark)") follow,
/// in Agent Auth's order.
fn usage_windows(windows: &[RemoteUsageWindow]) -> Vec<AgentUsageWindow> {
    let mut windows: Vec<&RemoteUsageWindow> = windows.iter().collect();
    windows.sort_by_key(|window| {
        let family = window.model_pattern.as_deref().is_none_or(|pattern| {
            pattern == "*" || pattern.strip_suffix("-*").is_some_and(|family| !family.contains('-'))
        });
        let label = window.label.as_deref().unwrap_or_default().to_ascii_lowercase();
        let weekly = label.contains("week") || label.contains('7');
        (!family, family && weekly)
    });
    windows
        .into_iter()
        .filter_map(|window| {
            let label = window.label.as_deref().map(str::trim).filter(|label| !label.is_empty())?;
            let used = window.used_fraction.filter(|used| used.is_finite())?;
            Some(AgentUsageWindow {
                label: label.to_owned(),
                used_fraction: used.clamp(0.0, 1.0) as f32,
                resets_at: window
                    .reset_at
                    .as_deref()
                    .or(window.resets_at.as_deref())
                    .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                    .map(|at| at.with_timezone(&Utc)),
            })
        })
        .collect()
}

/// No accounts, and why, for each agent.
fn unavailable(message: &str) -> AgentAccountsSnapshot {
    AgentAccountsSnapshot {
        accounts: Vec::new(),
        warnings: AGENTS
            .iter()
            .map(|harness| AgentAccountWarning {
                harness: *harness,
                message: message.to_owned(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::relay::tests::{AuthorityFixture, ProfileToken, static_token, test_control_plane_with_authority};
    use super::*;
    use crate::registry::HarnessRegistry;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use zeron_rpc::RpcService as _;

    async fn agent_auth() -> (String, Arc<AuthorityFixture>) {
        let fixture = AuthorityFixture::new(Utc::now() + chrono::TimeDelta::minutes(5));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(tx, fixture.clone()).await;
        (origin, fixture)
    }

    /// Agent Auth's list as an older server keeps it: a Claude login saved
    /// twice, a ChatGPT login saved by workspace and by user and workspace,
    /// a disabled login, and a Google one no managed agent runs.
    fn shared_pool() -> serde_json::Value {
        serde_json::json!([
            {"id": "claude-1", "provider": "anthropic", "providerAccountId": "uuid-1", "email": "one@example.com",
             "organization": "Team", "plan": "Max", "status": "connected", "usageWindows": [
                {"modelPattern": "claude-*", "label": "Weekly", "usedFraction": 0.5, "resetAt": "2030-06-29T00:00:00.000Z"},
                {"modelPattern": "claude-*", "label": "5-hour", "usedFraction": 1.4},
                {"modelPattern": "claude-*", "usedFraction": 0.1}
             ]},
            {"id": "claude-1-earlier", "provider": "anthropic", "providerAccountId": "uuid-1", "email": "one@example.com",
             "status": "attention_required"},
            {"id": "chatgpt-earlier", "provider": "openai", "providerAccountId": "ws-1", "email": "Person@Example.com",
             "status": "attention_required"},
            {"id": "chatgpt", "provider": "openai", "providerAccountId": "user-1::ws-1", "email": "person@example.com",
             "planLabel": "ChatGPT Pro", "status": "connected", "usageWindows": null},
            {"id": "claude-off", "provider": "anthropic", "providerAccountId": "uuid-2", "displayName": "Off",
             "status": "disabled"},
            {"id": "gemini", "provider": "antigravity", "providerAccountId": "g-1", "email": "g@example.com",
             "status": "connected"}
        ])
    }

    fn rows(snapshot: &serde_json::Value, harness: &str) -> Vec<serde_json::Value> {
        snapshot["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["harness"] == harness)
            .cloned()
            .collect()
    }

    fn ids(rows: &[serde_json::Value]) -> Vec<&str> {
        rows.iter().map(|row| row["id"].as_str().unwrap()).collect()
    }

    /// An engine core whose registry is managed by `crew`, served over
    /// upstream's own RPC surface (the seam in `rpc.rs`).
    fn managed_core(crew: &Arc<CrewInference>, dir: &std::path::Path) -> crate::EngineCore {
        let registry = Arc::new(HarnessRegistry::new());
        registry.set_crew_inference(crew.clone());
        crate::EngineCore::assemble(dir, registry, HarnessId::Mock, None).unwrap()
    }

    /// One account call answered by `crew` itself, as `rpc.rs` asks it.
    async fn account_call(
        crew: &CrewInference,
        accounts: &AgentAccounts,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        match crew.accounts_rpc(accounts, method, &params).await.expect("an account call") {
            Ok(RpcReply::Value(value)) => Ok(value),
            Ok(_) => panic!("{method} answers a value"),
            Err(error) => Err(error),
        }
    }

    fn state_of(url: &str) -> String {
        reqwest::Url::parse(url)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned()
    }

    /// A family's windows lead, its session before its week, whatever
    /// Agent Auth's order; a model's or tier's own follow in theirs.
    #[test]
    fn the_family_s_session_and_week_lead_the_windows() {
        let window = |pattern: &str, label: &str| RemoteUsageWindow {
            model_pattern: Some(pattern.into()),
            label: Some(label.into()),
            used_fraction: Some(0.5),
            ..RemoteUsageWindow::default()
        };
        let listed = [
            window("claude-opus-*", "Opus weekly"),
            window("claude-*", "Weekly"),
            window("claude-fable-*", "Fable weekly"),
            window("claude-*", "5-hour"),
        ];
        let labels: Vec<String> = usage_windows(&listed).into_iter().map(|w| w.label).collect();
        assert_eq!(labels, ["5-hour", "Weekly", "Opus weekly", "Fable weekly"]);
        let spark = [
            window("gpt-5.3-codex-spark", "Weekly (Spark)"),
            window("gpt-5.3-codex-spark", "5-hour (Spark)"),
            window("gpt-*", "Weekly"),
        ];
        let labels: Vec<String> = usage_windows(&spark).into_iter().map(|w| w.label).collect();
        assert_eq!(labels, ["Weekly", "Weekly (Spark)", "5-hour (Spark)"]);
    }

    /// `ListAgentAccounts` answers Crew's shared accounts: each login once,
    /// through a healthy copy, as Claude Code's, Codex's and OMP's rows;
    /// `ForgetAgentAccount` revokes the login with its copies and answers
    /// the accounts, and the chats' Account option follows.
    #[tokio::test]
    async fn a_managed_engine_lists_and_removes_crew_s_shared_accounts() {
        let (origin, fixture) = agent_auth().await;
        *lock(&fixture.accounts) = shared_pool();
        let crew = Arc::new(CrewInference::start(&origin, static_token()).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let core = managed_core(&crew, dir.path());
        let client = zeron_rpc::memory_client(core.rpc_service());

        let listed = client
            .call(methods::LIST_AGENT_ACCOUNTS, serde_json::json!({"forceUsage": false}))
            .await
            .unwrap();
        let snapshot: AgentAccountsSnapshot = serde_json::from_value(listed.clone()).unwrap();
        assert!(snapshot.warnings.is_empty());
        assert!(snapshot.accounts.iter().all(|row| !row.active && !row.switchable));
        assert!(snapshot.accounts.iter().all(|row| row.auth_kind == Some(AgentAuthKind::Oauth)));
        let claude = rows(&listed, "claude-code");
        assert_eq!(ids(&claude), ["claude-1", "claude-off"], "one row a login, a healthy copy shown");
        assert!(claude.iter().all(|row| row.get("provider").is_none()));
        assert_eq!(claude[0]["email"], "one@example.com");
        assert_eq!(claude[0]["planLabel"], "Max");
        assert_eq!(claude[0]["organization"], "Team");
        assert!(claude[0].get("usageError").is_none());
        let windows = &snapshot.accounts[0].usage_windows;
        assert_eq!(windows.len(), 2, "a window without a label isn't shown");
        // (The session first, as upstream's app reads it, though Agent Auth
        // listed the week first.)
        assert_eq!((windows[0].label.as_str(), windows[0].used_fraction, windows[0].resets_at), ("5-hour", 1.0, None));
        assert_eq!((windows[1].label.as_str(), windows[1].used_fraction), ("Weekly", 0.5));
        assert_eq!(windows[1].resets_at, Some("2030-06-29T00:00:00Z".parse().unwrap()));
        assert_eq!(claude[1]["usageError"], "Disabled");
        assert_eq!(claude[1]["displayName"], "Off");
        let codex = rows(&listed, "codex");
        assert_eq!(ids(&codex), ["chatgpt"]);
        assert_eq!(codex[0]["planLabel"], "ChatGPT Pro");
        assert_eq!(codex[0]["usageWindows"], serde_json::json!([]));
        let omp = rows(&listed, "omp");
        assert_eq!(ids(&omp), ["claude-1", "chatgpt", "claude-off"]);
        let groups: Vec<&str> = omp.iter().map(|row| row["provider"].as_str().unwrap()).collect();
        assert_eq!(groups, ["anthropic", "openai", "anthropic"]);
        assert!(!listed.to_string().contains("gemini"), "no managed agent runs a Google login");
        // A plain list is served from the minute's list.
        client.call(methods::LIST_AGENT_ACCOUNTS, serde_json::json!({})).await.unwrap();
        assert_eq!(fixture.accounts_listed.load(Ordering::SeqCst), 1);

        // Another agent's account isn't one of this one's to remove.
        let mismatched = client
            .call(methods::FORGET_AGENT_ACCOUNT, serde_json::json!({"harness": "claude-code", "accountId": "chatgpt"}))
            .await
            .unwrap_err();
        assert!(mismatched.to_string().contains("isn't one of Claude Code's"), "{mismatched}");
        assert!(lock(&fixture.revoked).is_empty());
        let after = client
            .call(methods::FORGET_AGENT_ACCOUNT, serde_json::json!({"harness": "codex", "accountId": "chatgpt"}))
            .await
            .unwrap();
        assert_eq!(*lock(&fixture.revoked), ["chatgpt", "chatgpt-earlier"], "the login with every copy");
        assert!(rows(&after, "codex").is_empty());
        assert_eq!(ids(&rows(&after, "omp")), ["claude-1", "claude-off"]);
        // The chats' Account option lists again too.
        let accounts = crew.accounts().await;
        assert!(accounts.iter().all(|account| !account.id.starts_with("chatgpt")));

        // A copy removed from OMP's row takes the whole login.
        client
            .call(methods::FORGET_AGENT_ACCOUNT, serde_json::json!({"harness": "omp", "accountId": "claude-1"}))
            .await
            .unwrap();
        assert_eq!(lock(&fixture.revoked)[2..], ["claude-1", "claude-1-earlier"]);

        // A list that fails is empty, saying why for each agent.
        fixture.fail_accounts.store(true, Ordering::SeqCst);
        let failed = client
            .call(methods::LIST_AGENT_ACCOUNTS, serde_json::json!({"forceUsage": true}))
            .await
            .unwrap();
        let failed: AgentAccountsSnapshot = serde_json::from_value(failed).unwrap();
        assert!(failed.accounts.is_empty());
        let agents: Vec<HarnessId> = failed.warnings.iter().map(|warning| warning.harness).collect();
        assert_eq!(agents, AGENTS);
        assert_eq!(
            failed.warnings[0].message,
            "Crew's shared accounts couldn't be loaded: Agent Auth is unavailable right now (temporarily unavailable)."
        );
        core.shutdown().await;
    }

    /// What shared accounts don't do is refused with a sentence; a sign-in
    /// starts as upstream's do (Claude's by pasted code); and another
    /// device's relayed call is refused.
    #[tokio::test]
    async fn managed_account_calls_start_shared_sign_ins_and_refuse_the_rest() {
        let (origin, fixture) = agent_auth().await;
        let crew = Arc::new(CrewInference::start(&origin, static_token()).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let core = managed_core(&crew, dir.path());
        let client = zeron_rpc::memory_client(core.rpc_service());
        let call = |method: &'static str, params: serde_json::Value| client.call(method, params);

        for (method, params, said) in [
            (methods::ACTIVATE_AGENT_ACCOUNT, serde_json::json!({"harness": "claude-code", "accountId": "acct-claude-1"}), CHOSEN_PER_CHAT),
            (methods::RENAME_AGENT_ACCOUNT, serde_json::json!({"harness": "codex", "accountId": "acct-openai-1", "name": "Mine"}), NAMED_BY_LOGIN),
            (methods::ADD_AGENT_API_KEY, serde_json::json!({"harness": "omp", "provider": "anthropic", "key": "sk-ant-test"}), NO_API_KEYS),
            (methods::VAULT_PUT_API_KEY, serde_json::json!({"provider": "anthropic-key", "key": "sk-ant-test", "authorizedDevices": []}), NO_API_KEYS),
            (methods::CONSUME_CODEX_RESET_CREDIT, serde_json::json!({"accountId": "acct-openai-1", "creditId": "c", "idempotencyKey": "k"}), NO_RESET_CREDITS),
        ] {
            let error = call(method, params).await.unwrap_err();
            assert_eq!(error.to_string(), said, "{method}");
        }

        for (params, mode) in [
            (serde_json::json!({"harness": "claude-code"}), "paste-code"),
            (serde_json::json!({"harness": "omp", "provider": "anthropic"}), "paste-code"),
        ] {
            let start = call(methods::START_AGENT_LOGIN, params).await.unwrap();
            assert_eq!(start["mode"], mode);
            assert!(start["url"].as_str().unwrap().starts_with("https://claude.ai/oauth/authorize?"));
            let login = serde_json::json!({"loginId": start["loginId"]});
            assert_eq!(call(methods::POLL_AGENT_LOGIN, login.clone()).await.unwrap()["status"], "pending");
            assert_eq!(call(methods::CANCEL_AGENT_LOGIN, login.clone()).await.unwrap()["ok"], true);
            let gone = call(methods::POLL_AGENT_LOGIN, login).await.unwrap_err();
            assert!(gone.to_string().contains("expired"), "{gone}");
        }
        for (params, said) in [
            (serde_json::json!({"harness": "cursor"}), "Claude Code, Codex and OMP"),
            (
                serde_json::json!({"harness": "claude-code", "provider": "openai"}),
                "Claude Code adds Claude logins to Crew's shared accounts, not \"openai\".",
            ),
            (serde_json::json!({"harness": "codex", "provider": "anthropic"}), "Codex adds ChatGPT logins"),
            (serde_json::json!({"harness": "omp"}), "Choose Claude or ChatGPT"),
            (serde_json::json!({"harness": "omp", "provider": "google"}), "OMP adds Claude or ChatGPT logins"),
        ] {
            let error = call(methods::START_AGENT_LOGIN, params.clone()).await.unwrap_err();
            assert!(error.to_string().contains(said), "{params}: {error}");
        }

        // Another device, through the relay: refused, list included.
        let rpc = core.rpc_service();
        for method in [methods::LIST_AGENT_ACCOUNTS, methods::START_AGENT_LOGIN, methods::FORGET_AGENT_ACCOUNT] {
            let params = serde_json::json!({"harness": "claude-code", "accountId": "acct-claude-1"});
            let relayed = zeron_rpc::with_caller(
                zeron_rpc::Caller::Peer { device: Some("laptop".into()) },
                rpc.handle(method, params),
            )
            .await;
            let Err(relayed) = relayed else {
                panic!("{method} answered another device");
            };
            assert_eq!(relayed.to_string(), LOCAL_ONLY, "{method}");
        }
        assert_eq!(fixture.accounts_listed.load(Ordering::SeqCst), 0);
        assert!(lock(&fixture.revoked).is_empty());
        core.shutdown().await;
    }

    /// A completed sign-in answers the accounts with the new one in them
    /// (listed again, the chats' Account option too). An import Agent Auth
    /// refuses keeps the tokens: the poll imports them without redeeming
    /// the code again.
    #[tokio::test]
    async fn a_completed_shared_sign_in_answers_the_accounts_it_joined() {
        let (origin, fixture) = agent_auth().await;
        let crew = CrewInference::start(&origin, static_token()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let accounts = AgentAccounts::for_crew_tests(dir.path(), &origin);
        let call = |method: &'static str, params: serde_json::Value| account_call(&crew, &accounts, method, params);
        // Listed once: the chats' Account option keeps it a minute.
        assert!(crew.accounts().await.iter().all(|account| account.id != "imported-1"));

        let start = call(methods::START_AGENT_LOGIN, serde_json::json!({"harness": "claude-code"})).await.unwrap();
        let code = format!("pasted-code#{}", state_of(start["url"].as_str().unwrap()));
        let login_id = start["loginId"].clone();
        let joined = call(methods::COMPLETE_AGENT_LOGIN, serde_json::json!({"loginId": login_id, "code": code}))
            .await
            .unwrap();
        let joined: AgentAccountsSnapshot = serde_json::from_value(joined).unwrap();
        let new: Vec<&AgentAccount> = joined.accounts.iter().filter(|row| row.id == "imported-1").collect();
        let harnesses: Vec<HarnessId> = new.iter().map(|row| row.harness).collect();
        assert_eq!(harnesses, [HarnessId::ClaudeCode, HarnessId::Omp]);
        assert_eq!(new[0].email.as_deref(), Some("new@example.com"));
        assert!(crew.accounts().await.iter().any(|account| account.id == "imported-1"), "listed again");
        let imported = lock(&fixture.imported)[0].clone();
        assert_eq!(imported["providerAccountId"], "acct-new");
        assert_eq!(fixture.redeemed.load(Ordering::SeqCst), 1);
        assert!(!dir.path().join("data").join("agent-accounts").exists(), "nothing saved on this device");

        // Agent Auth refuses the first import: the completion says so, and a
        // poll imports the same tokens; the code is redeemed once.
        fixture.refuse_imports.store(1, Ordering::SeqCst);
        let start = call(methods::START_AGENT_LOGIN, serde_json::json!({"harness": "omp", "provider": "anthropic"}))
            .await
            .unwrap();
        let state = state_of(start["url"].as_str().unwrap());
        let login = serde_json::json!({"loginId": start["loginId"], "code": format!("again#{state}")});
        let refused = call(methods::COMPLETE_AGENT_LOGIN, login.clone()).await.unwrap_err().to_string();
        assert_eq!(
            refused,
            "The account isn't in Crew's shared accounts yet: Agent Auth is unavailable right now \
             (temporarily unavailable). Try again in a moment."
        );
        let polled = call(methods::POLL_AGENT_LOGIN, serde_json::json!({"loginId": start["loginId"]})).await.unwrap();
        assert_eq!(polled["status"], "done");
        assert_eq!(fixture.redeemed.load(Ordering::SeqCst), 2, "one redemption a sign-in");
        let imports = lock(&fixture.imported).clone();
        assert_eq!(imports.len(), 3);
        assert_eq!(imports[1], imports[2], "the same tokens, retried");
        crew.shutdown();

        // Signed out of Crew: no sign-in starts, and the list says why.
        let token = Arc::new(ProfileToken::default());
        token.signed_out.store(true, Ordering::SeqCst);
        let signed_out = CrewInference::start(&origin, token).unwrap();
        let refused = account_call(&signed_out, &accounts, methods::START_AGENT_LOGIN, serde_json::json!({"harness": "claude-code"}))
            .await
            .unwrap_err();
        assert_eq!(refused.to_string(), SIGNED_OUT);
        let listed = account_call(&signed_out, &accounts, methods::LIST_AGENT_ACCOUNTS, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            listed["warnings"][0]["message"],
            "Crew's shared accounts couldn't be loaded: you're signed out of Crew."
        );
        signed_out.shutdown();
    }

    #[test]
    fn copies_of_one_login_show_as_one_working_account() {
        let account = |id: &str, provider: &str, provider_account_id: &str, email: &str, status: &str| {
            serde_json::from_value::<RemoteAgentAccount>(serde_json::json!({
                "id": id, "provider": provider, "providerAccountId": provider_account_id, "email": email, "status": status,
            }))
            .unwrap()
        };
        let shown = |accounts: &[RemoteAgentAccount]| -> Vec<(String, Vec<String>)> {
            shared_logins(accounts)
                .into_iter()
                .map(|login| (login.account.id.clone(), login.copies.iter().map(|copy| copy.id.clone()).collect()))
                .collect()
        };
        // Crew's Settings' cases.
        let chatgpt = [
            account("earlier", "openai", "workspace-1", "Person@Example.com", "attention_required"),
            account("other", "openai", "workspace-2", "other@example.com", "connected"),
            account("workspace-only", "openai", "workspace-1", "person@example.com", "connected"),
            account("crew", "openai", "user-1::workspace-1", "person@example.com", "connected"),
        ];
        assert_eq!(
            shown(&chatgpt),
            [("crew".into(), vec!["workspace-only".into(), "earlier".into()]), ("other".into(), vec![])]
        );
        let team = [
            account("a", "openai", "user-a::team", "a@example.com", "connected"),
            account("b", "openai", "user-b::team", "b@example.com", "connected"),
            account("c", "openai", "team", "c@example.com", "connected"),
        ];
        assert_eq!(shown(&team).len(), 3, "seats of one team stay apart");
        let claude = [
            account("claude-1", "anthropic", "uuid-1", "person@example.com", "connected"),
            account("claude-2", "anthropic", "uuid-2", "person@example.com", "connected"),
            account("claude-1-earlier-key", "anthropic", "uuid-1", "person@example.com", "connected"),
            account("chatgpt", "openai", "uuid-1", "person@example.com", "connected"),
        ];
        assert_eq!(
            shown(&claude),
            [
                ("claude-1".into(), vec!["claude-1-earlier-key".into()]),
                ("claude-2".into(), vec![]),
                ("chatgpt".into(), vec![])
            ]
        );
        assert_eq!(
            login_containing(&chatgpt, "earlier"),
            Some(("openai".into(), vec!["crew".into(), "workspace-only".into(), "earlier".into()]))
        );
        assert_eq!(login_containing(&chatgpt, "nobody"), None);
    }
}
