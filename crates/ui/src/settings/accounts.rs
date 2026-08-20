//! Settings → Accounts: the shared Agent Auth pool grouped by Claude Code and
//! Codex, with provider usage, one-time local migration, revoke, add-account
//! login flows, and the existing agent-version controls.
//!
//! Agent Auth is authoritative. The page never activates a local account,
//! manages credential slots, or targets another device.

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, Context, Entity, Hsla, SharedString, Subscription, Task, Window, div, prelude::*,
    px,
};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use comet_proto::{
    AgentAccount, AgentAccountStatus, AgentAccountsSnapshot, AgentLoginMode, AgentLoginPoll,
    AgentLoginStart, AgentLoginStatus, HarnessId,
};
use comet_rpc::methods;
use comet_update::HarnessStatus;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::motion::{AnimationExt as _, COMET_PULSE};
use crate::popover::{self, Loadable};
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Pure: usage meters + labels
// ---------------------------------------------------------------------------

pub const USAGE_WARN_FRACTION: f32 = 0.80;
pub const USAGE_CRITICAL_FRACTION: f32 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLevel {
    /// < 80% — indigo.
    Normal,
    /// ≥ 80% — amber.
    Warn,
    /// ≥ 95% — red.
    Critical,
}

/// Threshold classification of a usage fraction. Pure.
pub fn usage_level(fraction: f32) -> UsageLevel {
    if fraction >= USAGE_CRITICAL_FRACTION {
        UsageLevel::Critical
    } else if fraction >= USAGE_WARN_FRACTION {
        UsageLevel::Warn
    } else {
        UsageLevel::Normal
    }
}
fn account_status_label(account: &AgentAccount) -> &'static str {
    if account.migration_available {
        return "Ready to import";
    }
    match account.status {
        AgentAccountStatus::Connected => "Connected",
        AgentAccountStatus::Disabled => "Disabled",
        AgentAccountStatus::AttentionRequired => "Needs attention",
        AgentAccountStatus::Revoked => "Revoked",
        AgentAccountStatus::Unknown => "Needs attention",
    }
}

pub fn usage_color(level: UsageLevel, theme: &Theme) -> Hsla {
    match level {
        UsageLevel::Normal => theme.accent,
        UsageLevel::Warn => theme.warning,
        UsageLevel::Critical => theme.danger,
    }
}

/// Compact absolute reset moment (comet settings.agents.tsx `formatReset`):
/// a local clock time ("3:45 PM") when it lands within ~22h, else a short
/// weekday ("Mon"); the caller prefixes "resets ". Pure given `now`.
pub fn format_reset(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    use chrono::Local;
    let at = resets_at?;
    let local = at.with_timezone(&Local);
    Some(if at.signed_duration_since(now).num_hours() < 22 {
        format!("resets {}", local.format("%-I:%M %p"))
    } else {
        format!("resets {}", local.format("%a"))
    })
}

/// Provider cards in display order.
pub const PROVIDERS: [(HarnessId, &str); 2] = [
    (HarnessId::ClaudeCode, "Anthropic"),
    (HarnessId::Codex, "OpenAI"),
];

/// Accounts of one provider in authoritative server order. Pure.
pub fn provider_accounts(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
) -> Vec<&AgentAccount> {
    snapshot
        .accounts
        .iter()
        .filter(|account| account.harness == harness)
        .collect()
}

/// "vX.Y.Z" — tolerate an already-prefixed version string. Pure.
pub fn version_label(version: &str) -> String {
    if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    }
}

/// What the right edge of an agent-version row offers. Pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessAction<'a> {
    /// Installed agent is below Comet's compatibility floor — danger-toned,
    /// still clickable (the fix IS the update).
    UpdateRequired { latest: Option<&'a str> },
    /// A newer version exists.
    Update { latest: Option<&'a str> },
    /// Installed and current.
    UpToDate,
    /// Not installed — nothing actionable to show.
    Nothing,
}

/// Row action for one [`HarnessStatus`]: required beats available beats
/// up-to-date; an uninstalled agent shows nothing. Pure.
pub fn harness_action(status: &HarnessStatus) -> HarnessAction<'_> {
    if status.update_required {
        HarnessAction::UpdateRequired {
            latest: status.latest_version.as_deref(),
        }
    } else if status.update_available {
        HarnessAction::Update {
            latest: status.latest_version.as_deref(),
        }
    } else if status.installed_version.is_some() {
        HarnessAction::UpToDate
    } else {
        HarnessAction::Nothing
    }
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

enum LoginFlow {
    /// StartAgentLogin in flight.
    Starting { harness: HarnessId },
    /// Claude-style: open the URL, paste the code back.
    PasteCode {
        harness: HarnessId,
        start: AgentLoginStart,
        submitting: bool,
        error: Option<SharedString>,
    },
    /// Codex-style: open the URL, poll until the browser flow lands.
    Browser {
        harness: HarnessId,
        start: AgentLoginStart,
        message: Option<SharedString>,
        error: Option<SharedString>,
    },
}

impl LoginFlow {
    /// Dialog title for the provider account being added.
    fn title(&self) -> &'static str {
        let harness = match self {
            LoginFlow::Starting { harness }
            | LoginFlow::PasteCode { harness, .. }
            | LoginFlow::Browser { harness, .. } => *harness,
        };
        match harness {
            HarnessId::Codex => "Add OpenAI account",
            _ => "Add Anthropic account",
        }
    }
}

pub struct AccountsPage {
    state: Entity<AppState>,
    snapshot: Loadable<AgentAccountsSnapshot>,
    /// Account id with an in-flight migration or revoke.
    busy_account: Option<String>,
    /// Shared account awaiting explicit removal confirmation.
    pending_revoke: Option<AgentAccount>,
    login: Option<LoginFlow>,
    error: Option<SharedString>,
    code_input: Entity<ComposerInput>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    /// Harness ids with an in-flight `UpdateHarness` — self-updaters can take
    /// minutes, so each harness tracks its own flag (no timeout UI; the RPC
    /// resolves or fails).
    updating_harnesses: HashSet<String>,
    /// Last `UpdateHarness` failure per harness id — dismissed on retry.
    harness_errors: HashMap<String, SharedString>,
    /// One task per harness so concurrent updates don't cancel each other.
    harness_tasks: HashMap<String, Task<()>>,
    auto_update_task: Option<Task<()>>,
    _observe: Subscription,
    _code_events: Subscription,
}

impl AccountsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let code_input = cx.new(|cx| ComposerInput::new("Paste the authorization code", cx));
        let code_events = cx.subscribe(&code_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_code(cx);
            }
        });
        let mut page = Self {
            state,
            snapshot: Loadable::Idle,
            busy_account: None,
            pending_revoke: None,
            login: None,
            error: None,
            code_input,
            load_task: None,
            action_task: None,
            poll_task: None,
            updating_harnesses: HashSet::new(),
            harness_errors: HashMap::new(),
            harness_tasks: HashMap::new(),
            auto_update_task: None,
            _observe: observe,
            _code_events: code_events,
        };
        page.load(cx);
        page
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.snapshot = Loadable::Error("Engine not connected".into());
            return;
        };
        self.snapshot = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_AGENT_ACCOUNTS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.snapshot = match result {
                    Ok(value) => match serde_json::from_value::<AgentAccountsSnapshot>(value) {
                        Ok(snapshot) => Loadable::Ready(snapshot),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Migrate a local recovery snapshot or revoke a shared account.
    fn account_action(
        &mut self,
        method: &'static str,
        account: &AgentAccount,
        cx: &mut Context<Self>,
    ) {
        if self.busy_account.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy_account = Some(account.id.clone());
        self.error = None;
        let params = serde_json::json!({
            "accountId": account.id,
            "harness": account.harness,
        });
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.busy_account = None;
                match result {
                    Ok(_) => page.load(cx),
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn request_revoke(&mut self, account: AgentAccount, cx: &mut Context<Self>) {
        if self.busy_account.is_some() {
            return;
        }
        self.pending_revoke = Some(account);
        cx.notify();
    }

    fn cancel_revoke(&mut self, cx: &mut Context<Self>) {
        self.pending_revoke = None;
        cx.notify();
    }

    fn confirm_revoke(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.pending_revoke.take() else {
            return;
        };
        self.account_action(methods::REVOKE_AGENT_ACCOUNT, &account, cx);
    }

    // ---- add-account flows ----

    fn start_login(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.login = Some(LoginFlow::Starting { harness });
        self.error = None;
        let params = serde_json::json!({ "harness": harness });
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::START_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result.and_then(|value| {
                    serde_json::from_value::<AgentLoginStart>(value)
                        .map_err(|e| comet_rpc::RpcError::Failed(e.to_string()))
                }) {
                    Ok(start) => {
                        cx.open_url(&start.url);
                        match start.mode {
                            AgentLoginMode::PasteCode => {
                                page.code_input
                                    .update(cx, |input, cx| input.set_text("", cx));
                                page.login = Some(LoginFlow::PasteCode {
                                    harness,
                                    start,
                                    submitting: false,
                                    error: None,
                                });
                            }
                            AgentLoginMode::Browser => {
                                page.login = Some(LoginFlow::Browser {
                                    harness,
                                    start,
                                    message: None,
                                    error: None,
                                });
                                page.spawn_poll(cx);
                            }
                        }
                    }
                    Err(err) => {
                        page.login = None;
                        page.error = Some(format!("Login failed to start: {err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn submit_code(&mut self, cx: &mut Context<Self>) {
        let Some(LoginFlow::PasteCode {
            start, submitting, ..
        }) = &mut self.login
        else {
            return;
        };
        if *submitting {
            return;
        }
        let code = self.code_input.read(cx).text().trim().to_string();
        if code.is_empty() {
            return;
        }
        let login_id = start.login_id.clone();
        *submitting = true;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = serde_json::json!({ "loginId": login_id, "code": code });
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::COMPLETE_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.login = None;
                        page.load(cx);
                    }
                    Err(err) => {
                        if let Some(LoginFlow::PasteCode {
                            submitting, error, ..
                        }) = &mut page.login
                        {
                            *submitting = false;
                            *error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The browser-wait poll loop: PollAgentLogin every 1.5s until Done/Error.
    fn spawn_poll(&mut self, cx: &mut Context<Self>) {
        let Some(LoginFlow::Browser { start, .. }) = &self.login else {
            return;
        };
        let login_id = start.login_id.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = serde_json::json!({ "loginId": login_id });
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1500))
                    .await;
                let result = engine
                    .client()
                    .call(methods::POLL_AGENT_LOGIN, params.clone())
                    .await;
                let outcome = this.update(cx, |page, cx| {
                    let Some(LoginFlow::Browser { message, error, .. }) = &mut page.login else {
                        return true; // dialog dismissed — stop polling
                    };
                    match result.as_ref().ok().and_then(|value| {
                        serde_json::from_value::<AgentLoginPoll>(value.clone()).ok()
                    }) {
                        Some(poll) => match poll.status {
                            AgentLoginStatus::Done => {
                                page.login = None;
                                page.load(cx);
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Error => {
                                *error = Some(
                                    poll.message
                                        .unwrap_or_else(|| "Login failed".to_string())
                                        .into(),
                                );
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Pending => {
                                if let Some(text) = poll.message {
                                    *message = Some(text.into());
                                }
                                cx.notify();
                                false
                            }
                        },
                        None => {
                            let text = match &result {
                                Err(err) => format!("Poll failed: {err}"),
                                Ok(_) => "Poll failed: malformed reply".to_string(),
                            };
                            *error = Some(text.into());
                            cx.notify();
                            true
                        }
                    }
                });
                match outcome {
                    Ok(true) | Err(_) => break,
                    Ok(false) => {}
                }
            }
        }));
    }

    fn cancel_login(&mut self, cx: &mut Context<Self>) {
        let login_id = match &self.login {
            Some(LoginFlow::PasteCode { start, .. }) | Some(LoginFlow::Browser { start, .. }) => {
                Some(start.login_id.clone())
            }
            _ => None,
        };
        self.login = None;
        self.poll_task = None;
        if let (Some(login_id), Some(engine)) = (login_id, self.state.read(cx).engine().cloned()) {
            let params = serde_json::json!({ "loginId": login_id });
            self.action_task = Some(cx.spawn(async move |_, _| {
                if let Err(err) = engine
                    .client()
                    .call(methods::CANCEL_AGENT_LOGIN, params)
                    .await
                {
                    tracing::debug!(error = %err, "CancelAgentLogin failed (best-effort)");
                }
            }));
        }
        cx.notify();
    }

    /// Run one agent CLI's self-updater on the engine's device. The button
    /// reads "Updating…" until the RPC resolves; success just clears the flag
    /// — the authoritative refresh arrives over the `UpdateStatus` stream.
    fn update_harness(&mut self, id: String, cx: &mut Context<Self>) {
        if self.updating_harnesses.contains(&id) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.updating_harnesses.insert(id.clone());
        // Retry dismisses the previous failure.
        self.harness_errors.remove(&id);
        let params = serde_json::json!({ "harness": id });
        let task_id = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::UPDATE_HARNESS, params).await;
            this.update(cx, |page, cx| {
                page.updating_harnesses.remove(&task_id);
                if let Err(err) = result {
                    page.harness_errors
                        .insert(task_id.clone(), format!("{err}").into());
                }
                cx.notify();
            })
            .ok();
        });
        self.harness_tasks.insert(id, task);
        cx.notify();
    }

    /// Persist the "update agents after Crew updates" toggle. Optimistic:
    /// the local frame flips now, the next `UpdateStatus` frame confirms (or
    /// corrects, if the engine-side write failed).
    fn set_harness_auto_update(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.state.update(cx, |state, cx| {
            if let Some(update) = &mut state.update {
                update.harness_auto_update = enabled;
            }
            cx.notify();
        });
        let params = serde_json::json!({ "enabled": enabled });
        self.auto_update_task = Some(cx.spawn(async move |_, _| {
            if let Err(err) = engine
                .client()
                .call(methods::SET_HARNESS_AUTO_UPDATE, params)
                .await
            {
                tracing::debug!(error = %err, "SetHarnessAutoUpdate failed (next frame corrects)");
            }
        }));
        cx.notify();
    }

    // ---- render pieces ----

    /// One usage window (comet settings.agents.tsx `UsageMeter`): label ·
    /// 5px rounded-full bar (indigo → amber ≥80% → red ≥95%) · "NN% used" ·
    /// quiet reset time.
    fn render_usage_meter(
        &self,
        window: &comet_proto::AgentUsageWindow,
        theme: &Theme,
        now: DateTime<Utc>,
    ) -> AnyElement {
        let fraction = window.used_fraction.clamp(0.0, 1.0);
        let level = usage_level(fraction);
        let fill = usage_color(level, theme).opacity(match level {
            UsageLevel::Normal => 0.8,
            _ => 0.85,
        });
        let reset = format_reset(window.resets_at, now);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(px(11.5))
            .text_color(theme.text_muted.opacity(0.7))
            .child(
                div()
                    .w(px(48.0))
                    .flex_none()
                    .truncate()
                    .child(SharedString::from(window.label.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(56.0))
                    .max_w(px(230.0))
                    .h(px(5.0))
                    .rounded_full()
                    .overflow_hidden()
                    .bg(crate::theme::ink(0.07))
                    .when(fraction > 0.0, |el| {
                        el.child(
                            div()
                                .h_full()
                                // A 1.5% floor keeps tiny non-zero usage
                                // visible (comet `max(used, 1.5)%`).
                                .w(gpui::relative(fraction.max(0.015)))
                                .rounded_full()
                                .bg(fill),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(64.0))
                    .flex_none()
                    .text_right()
                    .child(SharedString::from(format!(
                        "{}% used",
                        (fraction * 100.0).round() as u32
                    ))),
            )
            .when_some(reset, |el, reset| {
                el.child(
                    div()
                        .flex_none()
                        .truncate()
                        .text_color(theme.text_muted.opacity(0.45))
                        .child(SharedString::from(reset)),
                )
            })
            .into_any_element()
    }

    /// One account row: identity and usage left; plan/status badges and the
    /// migrate or revoke action right-anchored.
    fn render_account_row(
        &self,
        account: &AgentAccount,
        ix: usize,
        first: bool,
        theme: &Theme,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::settings::widgets;
        let is_busy = self.busy_account.as_deref() == Some(account.id.as_str());
        let email: SharedString = account
            .email
            .clone()
            .or_else(|| account.display_name.clone())
            .unwrap_or_else(|| "Unknown account".into())
            .into();
        let initial: SharedString = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
            .into();
        let action_account = account.clone();

        let badges = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .when(account.migration_available, |el| {
                el.child(widgets::badge(theme, "Local"))
            })
            .when_some(account.plan_label.clone(), |el, plan| {
                el.child(widgets::badge(theme, plan))
            })
            .when(
                !account.migration_available
                    && account.status != AgentAccountStatus::Connected
                    && !account.usage_windows.is_empty(),
                |el| el.child(widgets::badge(theme, account_status_label(account))),
            );

        let actions = if account.migration_available {
            let label = if is_busy { "Importing…" } else { "Import" };
            crate::popover::btn_primary(theme, label)
                .id(("account-import", ix))
                .px(px(8.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .text_size(px(11.5))
                .when(is_busy, |el| el.opacity(0.5))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.account_action(methods::MIGRATE_AGENT_ACCOUNT, &action_account, cx);
                }))
                .into_any_element()
        } else {
            div()
                .id(("account-revoke", ix))
                .rounded(px(6.0))
                .px(px(6.0))
                .py(px(4.0))
                .text_color(theme.text_muted)
                .cursor_pointer()
                .when(is_busy, |el| el.opacity(0.5))
                .hover(|s| s.bg(crate::theme::ink(0.06)).text_color(theme.text))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.request_revoke(action_account.clone(), cx);
                }))
                .child(
                    crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
                .into_any_element()
        };

        div()
            .px(px(20.0))
            .py(px(14.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .flex()
            .flex_row()
            .items_stretch()
            .gap(px(12.0))
            .child(
                // Initial avatar: size-8 rounded-full border bg-white/[0.03].
                div()
                    .flex_none()
                    .self_center()
                    .size(px(32.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.03))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(initial),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, email))
                    .map(|el| {
                        if account.usage_windows.is_empty() {
                            el.child(
                                div()
                                    .mt(px(6.0))
                                    .truncate()
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted.opacity(0.6))
                                    .child(SharedString::from(
                                        if account.status == AgentAccountStatus::Connected
                                            && !account.migration_available
                                        {
                                            "Remaining usage unavailable"
                                        } else {
                                            account_status_label(account)
                                        },
                                    )),
                            )
                        } else {
                            el.child(
                                div().mt(px(6.0)).flex().flex_col().gap(px(4.0)).children(
                                    account
                                        .usage_windows
                                        .iter()
                                        .map(|w| self.render_usage_meter(w, theme, now)),
                                ),
                            )
                        }
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_end()
                    .justify_between()
                    .gap(px(8.0))
                    .child(badges)
                    .child(actions),
            )
            .into_any_element()
    }

    fn render_revoke_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.pending_revoke.as_ref()?;
        let theme = Theme::of(cx).clone();
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, "Remove this account?"))
            .child(div().mt(px(6.0)).child(popover::dialog_body(
                &theme,
                "New turns will stop using it immediately.",
            )))
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "account-revoke-cancel")
                            .id("account-revoke-cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_revoke(cx))),
                    )
                    .child(
                        popover::btn_danger(&theme, "Remove")
                            .id("account-revoke-confirm")
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_revoke(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal("remove-account-dialog", viewport, card))
    }

    fn render_login_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let red_text = theme.danger_muted.opacity(0.9); // red-300
        let login = self.login.as_ref()?;
        let title = login.title();
        let url_link =
            |id: &'static str, label: &'static str, url: &str, cx: &mut Context<Self>| {
                let open_url = url.to_string();
                // "Reopen the …" text link (comet: `text-[12px]
                // text-muted-foreground/60 hover:underline`).
                div()
                    .id(id)
                    .mt(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .truncate()
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.open_url(&open_url);
                    }))
                    .child(SharedString::from(label))
            };
        let body: AnyElement = match login {
            LoginFlow::Starting { .. } => div()
                .mt(px(8.0))
                .child(popover::skeleton_rows(
                    "login-starting",
                    &theme,
                    2,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            LoginFlow::PasteCode {
                start,
                submitting,
                error,
                ..
            } => {
                let submitting = *submitting;
                div()
                    .flex()
                    .flex_col()
                    .child(div().mt(px(8.0)).child(popover::dialog_body(
                        &theme,
                        "Sign in to the account you want to add, approve access, then paste the \
                         code from Anthropic. Your current CLI login stays unchanged.",
                    )))
                    .child(url_link(
                        "login-open-url",
                        "Reopen the authorization page",
                        &start.url,
                        cx,
                    ))
                    .child(
                        div().mt(px(12.0)).child(
                            popover::dialog_field(self.code_input.clone().into_any_element())
                                .font_family(theme.font_mono.clone())
                                .text_size(px(13.0)),
                        ),
                    )
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(12.0))
                                .text_color(red_text)
                                .child(message),
                        )
                    })
                    .child(
                        div()
                            .mt(px(16.0))
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                popover::btn_ghost(&theme, "Cancel", "login-cancel")
                                    .id("login-cancel")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
                            )
                            .child(
                                popover::btn_primary(
                                    &theme,
                                    if submitting {
                                        "Verifying…"
                                    } else {
                                        "Add account"
                                    },
                                )
                                .id("login-submit-code")
                                .when(submitting, |el| el.opacity(0.5))
                                .on_click(cx.listener(|this, _, _, cx| this.submit_code(cx))),
                            ),
                    )
                    .into_any_element()
            }
            LoginFlow::Browser {
                start,
                message,
                error,
                ..
            } => {
                let has_error = error.is_some();
                div()
                    .flex()
                    .flex_col()
                    .child(div().mt(px(8.0)).child(popover::dialog_body(
                        &theme,
                        "Finish signing in to OpenAI. The account is added to the shared pool; \
                         your current CLI login stays unchanged.",
                    )))
                    .child(url_link(
                        "login-open-url-browser",
                        "Reopen the sign-in page",
                        &start.url,
                        cx,
                    ))
                    .when(!has_error, |el| {
                        el.child(
                            div()
                                .mt(px(16.0))
                                .text_size(px(12.5))
                                .text_color(theme.text_muted)
                                .child(
                                    div()
                                        .child(message.clone().unwrap_or_else(|| {
                                            SharedString::from("Waiting for the browser…")
                                        }))
                                        .with_animation(
                                            "login-waiting",
                                            COMET_PULSE.animation().repeat(),
                                            |label, delta| label.opacity(0.55 + 0.35 * delta),
                                        ),
                                ),
                        )
                    })
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(12.0))
                                .text_size(px(12.0))
                                .text_color(red_text)
                                .child(message),
                        )
                    })
                    .child(
                        div().mt(px(16.0)).flex().flex_row().justify_end().child(
                            popover::btn_ghost(
                                &theme,
                                if has_error { "Close" } else { "Cancel" },
                                "login-cancel",
                            )
                            .id("login-cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
                        ),
                    )
                    .into_any_element()
            }
        };
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, title))
            .child(body)
            .into_any_element();
        Some(popover::modal("add-account-dialog", viewport, card))
    }

    /// A ghost account row (comet settings.agents.tsx `SkeletonRow`): avatar,
    /// email line, two usage-meter ghosts, a badge — same geometry as the real
    /// row so loaded data lands without a layout jump. `dim` fades row two.
    fn render_skeleton_row(
        &self,
        _id: (&'static str, usize),
        dim: bool,
        first: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::motion;
        let delta = motion::pulse_delta(&motion::COMET_PULSE, cx.entity_id(), cx);
        let ghost = |w: gpui::Length, h: f32, round_full: bool| {
            div()
                .w(w)
                .h(px(h))
                .flex_none()
                .map(|el| {
                    if round_full {
                        el.rounded_full()
                    } else {
                        el.rounded(px(4.0))
                    }
                })
                .bg(crate::theme::ink(0.05))
        };
        let meters = div()
            .mt(px(8.0))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .children((0..2).map(|_| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(ghost(px(48.0).into(), 9.0, false))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(56.0))
                            .max_w(px(230.0))
                            .h(px(5.0))
                            .rounded_full()
                            .bg(crate::theme::ink(0.04)),
                    )
                    .child(ghost(px(64.0).into(), 9.0, false))
            }));
        let inner = div()
            .flex()
            .flex_row()
            .items_stretch()
            .gap(px(12.0))
            .child(
                div()
                    .flex_none()
                    .self_center()
                    .size(px(32.0))
                    .rounded_full()
                    .bg(crate::theme::ink(0.05)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(ghost(px(176.0).into(), 13.0, false).max_w(gpui::relative(0.6)))
                    .child(meters),
            )
            .child(div().flex_none().flex().flex_col().items_end().child(ghost(
                px(64.0).into(),
                21.0,
                true,
            )));
        div()
            .px(px(20.0))
            .py(px(14.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .when(dim, |el| el.opacity(0.6))
            .child(inner.opacity(0.55 + 0.35 * motion::pulse_wave(delta)))
            .into_any_element()
    }

    /// One agent-version row: name, installed version (or "Not installed"),
    /// the resolved executable path, and the right-anchored update affordance
    /// — same geometry and hairlines as the account rows above.
    fn render_harness_row(
        &self,
        harness: &HarnessStatus,
        ix: usize,
        first: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::settings::widgets;
        let updating = self.updating_harnesses.contains(&harness.id);
        let error = self.harness_errors.get(&harness.id).cloned();
        let name: SharedString = harness.name.clone().into();
        let version: SharedString = match &harness.installed_version {
            Some(version) => version_label(version).into(),
            None => "Not installed".into(),
        };
        let version_tone = theme
            .text_muted
            .opacity(if harness.installed_version.is_some() {
                0.65
            } else {
                0.5
            });

        // The right-edge affordance. "Update required" stays clickable — the
        // update IS the fix — but wears the page's danger tone (error_strip
        // palette) because the installed agent is below Comet's floor.
        let control: Option<AnyElement> = match harness_action(harness) {
            HarnessAction::UpdateRequired { latest } => {
                let label: SharedString = if updating {
                    "Updating…".into()
                } else {
                    match latest {
                        Some(v) => format!("Update required — {}", version_label(v)).into(),
                        None => "Update required".into(),
                    }
                };
                let id = harness.id.clone();
                Some(
                    div()
                        .id(("harness-update-required", ix))
                        .rounded(px(6.0))
                        .px(px(8.0))
                        .py(px(4.0))
                        .border_1()
                        .border_color(theme.danger.opacity(0.2))
                        .bg(theme.danger.opacity(0.06))
                        .text_size(px(11.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.danger_muted.opacity(0.9))
                        .cursor_pointer()
                        .when(updating, |el| el.opacity(0.5))
                        .hover(|s| s.bg(theme.danger.opacity(0.12)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.update_harness(id.clone(), cx);
                        }))
                        .child(label)
                        .into_any_element(),
                )
            }
            HarnessAction::Update { latest } => {
                let label = if updating {
                    "Updating…".to_string()
                } else {
                    match latest {
                        Some(v) => format!("Update to {}", version_label(v)),
                        None => "Update".to_string(),
                    }
                };
                let id = harness.id.clone();
                Some(
                    crate::popover::btn_primary(theme, &label)
                        .id(("harness-update", ix))
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .text_size(px(11.5))
                        .when(updating, |el| el.opacity(0.5))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.update_harness(id.clone(), cx);
                        }))
                        .into_any_element(),
                )
            }
            HarnessAction::UpToDate => Some(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted.opacity(0.55))
                    .child(SharedString::from("Up to date"))
                    .into_any_element(),
            ),
            HarnessAction::Nothing => None,
        };

        div()
            .px(px(20.0))
            .py(px(14.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, name))
                    .child(
                        div()
                            .mt(px(3.0))
                            .text_size(px(11.5))
                            .text_color(version_tone)
                            .child(version),
                    )
                    .when_some(harness.path.clone(), |el, path| {
                        el.child(
                            div()
                                .mt(px(2.0))
                                .truncate()
                                .text_size(px(10.5))
                                .font_family(theme.font_mono.clone())
                                .text_color(theme.text_muted.opacity(0.4))
                                .child(SharedString::from(path)),
                        )
                    })
                    .when_some(error, |el, message| {
                        el.child(
                            div()
                                .mt(px(4.0))
                                .text_size(px(11.5))
                                .text_color(theme.danger_muted.opacity(0.85))
                                .child(message),
                        )
                    }),
            )
            .children(control.map(|control| div().flex_none().child(control)))
            .into_any_element()
    }

    /// The "Update agents after Crew updates" row: title + description left,
    /// a 36×20 pill toggle right (the advisor settings toggle idiom).
    fn render_auto_update_row(
        &self,
        enabled: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .px(px(20.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(theme.border)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(20.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from("Update agents after Crew updates")),
                    )
                    .child(
                        div()
                            .mt(px(3.0))
                            .text_size(px(11.5))
                            .line_height(px(17.0))
                            .text_color(theme.text_muted.opacity(0.7))
                            .child(SharedString::from(
                                "Runs each agent's own updater whenever Comet finishes \
                                 updating itself.",
                            )),
                    ),
            )
            .child(
                div()
                    .id("harness-auto-update")
                    .flex_none()
                    .w(px(36.0))
                    .h(px(20.0))
                    .p(px(2.0))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .when(enabled, |toggle| toggle.justify_end().bg(theme.accent))
                    .when(!enabled, |toggle| {
                        toggle.justify_start().bg(crate::theme::ink(0.12))
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_harness_auto_update(!enabled, cx);
                    }))
                    .child(
                        div()
                            .size(px(16.0))
                            .rounded_full()
                            .bg(theme.bg)
                            .border_1()
                            .border_color(theme.border),
                    ),
            )
            .into_any_element()
    }

    /// The "Agent versions" section: one row per harness from the engine's
    /// `UpdateStatus` stream, then the auto-update toggle. Absent status or
    /// an empty list renders nothing.
    fn render_versions_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        use crate::settings::widgets;
        let update = self.state.read(cx).update.clone()?;
        if update.harnesses.is_empty() {
            return None;
        }
        let rows: Vec<AnyElement> = update
            .harnesses
            .iter()
            .enumerate()
            .map(|(ix, harness)| self.render_harness_row(harness, ix, ix == 0, theme, cx))
            .collect();
        Some(
            div()
                .mt(px(24.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from("Agent versions")),
                )
                .child(
                    widgets::section_card(theme)
                        .mt(px(8.0))
                        .children(rows)
                        .child(self.render_auto_update_row(update.harness_auto_update, theme, cx)),
                )
                .into_any_element(),
        )
    }
}

impl Render for AccountsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let dialog = if self.pending_revoke.is_some() {
            self.render_revoke_dialog(window.viewport_size(), cx)
        } else {
            self.render_login_dialog(window.viewport_size(), cx)
        };
        let refreshing = matches!(self.snapshot, Loadable::Loading);
        let account_count = self
            .snapshot
            .ready()
            .map(|s| s.accounts.len())
            .filter(|&n| n > 0);

        let provider_icon = |harness: HarnessId| match harness {
            HarnessId::Codex => (crate::icons::OPENAI_MARK, None),
            HarnessId::Cursor => (crate::icons::CURSOR_MARK, None),
            _ => (
                crate::icons::CLAUDE_MARK,
                Some(crate::icons::claude_brand()),
            ),
        };
        // Brand mark inside a 24px centered box (comet: `grid size-6
        // place-items-center [&_svg]:size-4`).
        let provider_mark = |harness: HarnessId, theme: &Theme| {
            let (mark, tint) = provider_icon(harness);
            div()
                .flex_none()
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    crate::icons::icon(mark)
                        .size(px(16.0))
                        .text_color(tint.unwrap_or(theme.text_muted)),
                )
        };

        // One section per provider (comet settings.agents.tsx `ProviderSection`):
        // brand header + Add account, then the account rows card.
        let sections: Vec<AnyElement> = match &self.snapshot {
            Loadable::Idle | Loadable::Loading => PROVIDERS
                .into_iter()
                .map(|(harness, name)| {
                    let skeleton_id = match harness {
                        HarnessId::Codex => "accounts-skeleton-codex",
                        _ => "accounts-skeleton-claude",
                    };
                    div()
                        .mt(px(24.0))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(provider_mark(harness, &theme))
                                .child(
                                    div()
                                        .text_size(px(14.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(SharedString::from(name)),
                                ),
                        )
                        .child(
                            // Ghost rows shaped like real ones (row two dimmed)
                            // so the card keeps its size while data develops.
                            widgets::section_card(&theme)
                                .mt(px(8.0))
                                .child(self.render_skeleton_row(
                                    (skeleton_id, 0),
                                    false,
                                    true,
                                    &theme,
                                    cx,
                                ))
                                .child(self.render_skeleton_row(
                                    (skeleton_id, 1),
                                    true,
                                    false,
                                    &theme,
                                    cx,
                                )),
                        )
                        .into_any_element()
                })
                .collect(),
            Loadable::Error(message) => {
                let message = message.clone();
                vec![
                    widgets::error_strip(&theme, message)
                        .id("accounts-load-error")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.load(cx)))
                        .child(
                            div()
                                .mt(px(4.0))
                                .text_size(px(11.5))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("Click to retry")),
                        )
                        .into_any_element(),
                ]
            }
            Loadable::Ready(snapshot) => {
                let snapshot = snapshot.clone();
                PROVIDERS
                    .into_iter()
                    .map(|(harness, name)| {
                        let accounts = provider_accounts(&snapshot, harness);
                        // EVERY warning renders its own strip (comet maps them).
                        let warnings: Vec<String> = snapshot
                            .warnings
                            .iter()
                            .filter(|w| w.harness == harness)
                            .map(|w| w.message.clone())
                            .collect();
                        let rows: Vec<AnyElement> = accounts
                            .iter()
                            .enumerate()
                            .map(|(ix, account)| {
                                self.render_account_row(account, ix, ix == 0, &theme, now, cx)
                            })
                            .collect();
                        let add_id: SharedString = format!("add-account-{name}").into();
                        let card = widgets::section_card(&theme).mt(px(8.0));
                        let card = if rows.is_empty() {
                            card.child(
                                div()
                                    .px(px(20.0))
                                    .py(px(32.0))
                                    .text_center()
                                    .text_size(px(14.0))
                                    .text_color(theme.text_muted.opacity(0.6))
                                    .child(SharedString::from(format!("No {name} accounts."))),
                            )
                        } else {
                            card.children(rows)
                        };
                        div()
                            .mt(px(24.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(provider_mark(harness, &theme))
                                    .child(
                                        div()
                                            .text_size(px(14.0))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(theme.text)
                                            .child(SharedString::from(name)),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        widgets::ghost_action(&theme)
                                            .id(add_id)
                                            .hover(|s| widgets::ghost_hover(&theme, s))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.start_login(harness, cx);
                                            }))
                                            .child(
                                                crate::icons::icon(crate::icons::ADD_CIRCLE)
                                                    .size(px(16.0))
                                                    .text_color(theme.text_muted),
                                            )
                                            .child(SharedString::from("Add account")),
                                    ),
                            )
                            .children(
                                warnings
                                    .into_iter()
                                    .map(|warning| widgets::warning_strip(&theme, warning)),
                            )
                            .child(card)
                            .into_any_element()
                    })
                    .collect()
            }
        };
        let versions = self.render_versions_section(&theme, cx);

        div()
            .id("accounts-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(widgets::page_header(&theme, "Accounts", account_count))
                            .child(div().flex_1())
                            .child(
                                // `text-[12.5px]` + leading 16px Refresh icon,
                                // dimmed while a refresh is in flight (comet
                                // `disabled:opacity-50`).
                                widgets::ghost_action(&theme)
                                    .id("accounts-refresh")
                                    .flex_none()
                                    .text_size(px(12.5))
                                    .hover(|s| widgets::ghost_hover(&theme, s))
                                    .when(refreshing, |el| el.opacity(0.5))
                                    .on_click(cx.listener(|this, _, _, cx| this.load(cx)))
                                    .child(
                                        crate::icons::icon(crate::icons::REFRESH)
                                            .size(px(16.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from("Refresh")),
                            ),
                    )
                    .child(widgets::page_subtitle(
                        &theme,
                        "One account pool for local agents and Scaffold.",
                    ))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(&theme, message)
                                .id("accounts-action-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .children(sections)
                    .children(versions)
                    // Footer note (comet: `mt-6 text-[12px] leading-relaxed
                    // text-muted-foreground/60`).
                    .child(
                        div()
                            .mt(px(24.0))
                            .text_size(px(12.0))
                            .line_height(px(19.0))
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(
                                "Accounts are encrypted centrally. Agents receive only short-lived, \
                                 session-scoped grants.",
                            )),
                    ),
            )
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn usage_thresholds_match_comet() {
        assert_eq!(usage_level(0.0), UsageLevel::Normal);
        assert_eq!(usage_level(0.79), UsageLevel::Normal);
        assert_eq!(usage_level(0.80), UsageLevel::Warn);
        assert_eq!(usage_level(0.94), UsageLevel::Warn);
        assert_eq!(usage_level(0.95), UsageLevel::Critical);
        assert_eq!(usage_level(1.0), UsageLevel::Critical);
    }

    #[test]
    fn usage_colors_map_to_theme_accents() {
        let theme = Theme::dark();
        assert_eq!(usage_color(UsageLevel::Normal, &theme), theme.accent);
        assert_eq!(usage_color(UsageLevel::Warn, &theme), theme.warning);
        assert_eq!(usage_color(UsageLevel::Critical, &theme), theme.danger);
    }

    #[test]
    fn reset_formatting_is_absolute() {
        use chrono::Local;
        let now = Utc::now();
        assert_eq!(format_reset(None, now), None);
        // Within ~22h: a local clock time ("resets 3:45 PM").
        let soon = now + TimeDelta::minutes(125);
        assert_eq!(
            format_reset(Some(soon), now),
            Some(format!(
                "resets {}",
                soon.with_timezone(&Local).format("%-I:%M %p")
            ))
        );
        // Beyond: a short weekday ("resets Mon").
        let later = now + TimeDelta::days(3);
        assert_eq!(
            format_reset(Some(later), now),
            Some(format!(
                "resets {}",
                later.with_timezone(&Local).format("%a")
            ))
        );
    }

    #[test]
    fn provider_grouping_preserves_authoritative_order() {
        let account = |id: &str, harness: HarnessId, migration_available: bool| AgentAccount {
            id: id.into(),
            harness,
            email: None,
            plan_label: None,
            status: AgentAccountStatus::Connected,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            migration_available,
        };
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account("c1", HarnessId::ClaudeCode, false),
                account("x1", HarnessId::Codex, false),
                account("c2", HarnessId::ClaudeCode, true),
            ],
            warnings: vec![],
        };
        let claude = provider_accounts(&snapshot, HarnessId::ClaudeCode);
        let ids: Vec<&str> = claude.iter().map(|account| account.id.as_str()).collect();
        assert_eq!(ids, ["c1", "c2"]);
        assert_eq!(provider_accounts(&snapshot, HarnessId::Codex).len(), 1);
        assert!(provider_accounts(&snapshot, HarnessId::Cursor).is_empty());
    }

    #[test]
    fn account_status_labels_distinguish_health_from_usage() {
        let mut account = AgentAccount {
            id: "account-1".into(),
            harness: HarnessId::Codex,
            email: None,
            plan_label: None,
            status: AgentAccountStatus::Connected,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            migration_available: false,
        };
        assert_eq!(account_status_label(&account), "Connected");
        account.status = AgentAccountStatus::AttentionRequired;
        assert_eq!(account_status_label(&account), "Needs attention");
        account.migration_available = true;
        assert_eq!(account_status_label(&account), "Ready to import");
    }

    #[test]
    fn version_labels_are_v_prefixed_once() {
        assert_eq!(version_label("1.2.3"), "v1.2.3");
        assert_eq!(version_label("v1.2.3"), "v1.2.3");
    }

    #[test]
    fn harness_action_precedence() {
        let status = |installed: Option<&str>, available: bool, required: bool| HarnessStatus {
            id: "omp".into(),
            name: "OMP".into(),
            path: installed.map(|_| "/usr/local/bin/omp".into()),
            installed_version: installed.map(str::to_string),
            latest_version: Some("2.0.0".into()),
            update_available: available,
            update_required: required,
        };
        // Required wins even when available is also set.
        assert_eq!(
            harness_action(&status(Some("1.0.0"), true, true)),
            HarnessAction::UpdateRequired {
                latest: Some("2.0.0")
            }
        );
        assert_eq!(
            harness_action(&status(Some("1.0.0"), true, false)),
            HarnessAction::Update {
                latest: Some("2.0.0")
            }
        );
        assert_eq!(
            harness_action(&status(Some("2.0.0"), false, false)),
            HarnessAction::UpToDate
        );
        // Not installed → nothing actionable on the right edge.
        assert_eq!(
            harness_action(&status(None, false, false)),
            HarnessAction::Nothing
        );
    }
}
