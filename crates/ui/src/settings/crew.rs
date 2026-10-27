//! Shared accounts and sandbox controls, with one durable command owner.
use super::widgets;
use crate::{composer::ComposerInput, state::AppState, theme::Theme};
use gpui::{Context, Entity, SharedString, Subscription, Window, div, prelude::*, px};
use serde::Deserialize;
use serde_json::{Value, json};
use zeron_proto::{
    AgentProvider, AgentRoute, CollaborationScope, ScaffoldDatabaseEnvironment,
    ScaffoldEnvironmentControl, ScaffoldEnvironmentControlResult, ScaffoldEnvironmentSnapshot,
    SessionEnvironmentSource,
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CrewStatus {
    pub(crate) scope: CollaborationScope,
    pub(crate) principal_id: String,
    pub(crate) role: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SharedAccount {
    id: String,
    provider: String,
    email: Option<String>,
    display_name: Option<String>,
    plan_label: Option<String>,
    status: String,
    #[serde(default)]
    usage_windows: Vec<zeron_proto::AgentUsageWindow>,
}

#[derive(Deserialize)]
struct Accounts {
    accounts: Vec<SharedAccount>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Outcome {
    pub(crate) command_id: String,
    pub(crate) status: String,
    pub(crate) result: Option<ScaffoldEnvironmentControlResult>,
    pub(crate) error: Option<String>,
}

pub(crate) fn attachment_after_create(
    outcome: &Outcome,
) -> Option<(String, ScaffoldEnvironmentControl)> {
    if outcome.status != "confirmed" || !outcome.command_id.ends_with(":create") {
        return None;
    }
    let result = outcome.result.as_ref()?;
    let SessionEnvironmentSource::Scaffold { sandbox_id, .. } = &result.environment.source else {
        return None;
    };
    Some((
        format!("{}:attach", outcome.command_id),
        ScaffoldEnvironmentControl::Attach {
            sandbox_id: sandbox_id.clone(),
            scope: result.environment.scope.clone(),
        },
    ))
}

pub(crate) fn attached_session(outcome: &Outcome) -> Option<(&str, &str)> {
    if outcome.status != "confirmed" {
        return None;
    }
    let result = outcome.result.as_ref()?;
    result.control_grant.as_ref()?;
    Some((
        result.environment.scope.session_id.as_deref()?,
        result.attached_device_id.as_deref()?,
    ))
}

#[cfg(test)]
mod provisioning_tests {
    use super::*;

    #[test]
    fn create_requires_a_distinct_durable_attach_and_unknown_never_advances() {
        let mut outcome: Outcome = serde_json::from_value(json!({
            "commandId": "one:create", "status": "confirmed", "result": {
                "environment": { "source": { "kind": "scaffold", "sandbox_id": "sandbox", "lifecycle": "starting" },
                    "ownerPrincipal": "owner", "scope": { "projectId": "project", "sessionId": "session" } }
            }
        })).unwrap();
        let (id, control) = attachment_after_create(&outcome).unwrap();
        assert_eq!(id, "one:create:attach");
        assert!(
            matches!(control, ScaffoldEnvironmentControl::Attach { sandbox_id, scope } if sandbox_id == "sandbox" && scope.session_id.as_deref() == Some("session"))
        );
        assert!(attached_session(&outcome).is_none());
        outcome.status = "unknown".into();
        assert!(attachment_after_create(&outcome).is_none());
        assert!(attached_session(&outcome).is_none());
        outcome.command_id = id;
        outcome.status = "confirmed".into();
        let result = outcome.result.as_mut().unwrap();
        result.attached_device_id = Some("sandbox-host".into());
        assert!(attached_session(&outcome).is_none());
        outcome.result.as_mut().unwrap().control_grant = Some(zeron_proto::ScaffoldControlGrant {
            id: "grant".into(),
            expires_at: i64::MAX,
            capabilities: vec![],
        });
        assert_eq!(
            attached_session(&outcome),
            Some(("session", "sandbox-host"))
        );
        assert!(attachment_after_create(&outcome).is_none());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrewView {
    Accounts,
    Sandboxes,
    Annotations,
}

pub(crate) struct AddAccount(pub zeron_proto::HarnessId);
impl gpui::EventEmitter<AddAccount> for CrewAccounts {}

pub struct CrewAccounts {
    state: Entity<AppState>,
    view: CrewView,
    annotations: Entity<super::annotations::Annotations>,
    status: Option<CrewStatus>,
    accounts: Vec<SharedAccount>,
    environments: Vec<zeron_proto::SessionEnvironment>,
    outcome: Option<Outcome>,
    busy: bool,
    error: Option<String>,
    notice: Option<String>,
    confirmation: Option<(&'static str, Value, String)>,
    model_picker: Entity<crate::pickers::Pickers>,
    _model_picked: Subscription,
    model: Option<String>,
    models: Vec<zeron_proto::Model>,
    account: Option<String>,
    account_select: widgets::SelectState,
    database: ScaffoldDatabaseEnvironment,
    database_select: widgets::SelectState,
    sandbox_options: bool,
    managed_sandbox: Option<String>,
    _observe: Subscription,
}

impl CrewAccounts {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let model_picker =
            cx.new(|cx| crate::pickers::Pickers::new_for_sandboxes(state.clone(), cx));
        let model_picked = cx.subscribe(
            &model_picker,
            |page: &mut Self, _, event: &crate::pickers::TitleModelPicked, cx| {
                page.model = event.0.model.clone();
                page.account = None;
                cx.notify();
            },
        );
        let mut page = Self {
            annotations: cx.new(|cx| super::annotations::Annotations::new(state.clone(), cx)),
            state,
            view: CrewView::Accounts,
            status: None,
            accounts: vec![],
            environments: vec![],
            outcome: None,
            busy: false,
            error: None,
            notice: None,
            confirmation: None,
            model_picker,
            _model_picked: model_picked,
            model: None,
            models: vec![],
            account: None,
            account_select: widgets::SelectState::default(),
            database: ScaffoldDatabaseEnvironment::Local,
            database_select: widgets::SelectState::default(),
            sandbox_options: false,
            managed_sandbox: None,
            _observe: observe,
        };
        if page.state.read(cx).managed_inference {
            page.refresh(cx);
        }
        page
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine unavailable. Reconnect, then refresh.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.error = None;
        let refresh_environments = self.view == CrewView::Sandboxes;
        cx.spawn(async move |this, cx| {
            let result = async {
                let status: CrewStatus = serde_json::from_value(engine.client().call("crew.status", json!({})).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
                let accounts = engine.client().call("crew.accounts.list", json!({})).await.map_err(|e| e.to_string())
                    .and_then(|value| serde_json::from_value::<Accounts>(value).map_err(|e| e.to_string()));
                let environments = if refresh_environments {
                    Some(engine.client().call("scaffold.refresh", json!(status.scope)).await.map_err(|e| e.to_string())
                        .and_then(|value| serde_json::from_value::<ScaffoldEnvironmentSnapshot>(value).map_err(|e| e.to_string())))
                } else { None };
                let models = if refresh_environments {
                    Some(engine.client().call(zeron_rpc::methods::LIST_MODELS, json!({"harness": zeron_proto::HarnessId::Omp})).await.map_err(|error| error.to_string())
                        .and_then(|value| serde_json::from_value::<Vec<zeron_proto::Model>>(value).map_err(|error| error.to_string())))
                } else { None };
                Ok::<_, String>((status, accounts, environments, models))
            }.await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok((status, accounts, environments, models)) => {
                        let recovery_key = json!([status.principal_id, status.scope]).to_string();
                        if page.outcome.is_none() {
                            if let Some(id) = super::current(cx).crew_recovery_commands.get(&recovery_key).cloned() {
                                page.outcome = Some(Outcome { command_id: id, status: "unknown".into(), result: None, error: Some("Recovered command; reconcile before issuing another operation.".into()) });
                            }
                        }
                        page.status = Some(status);
                        let mut errors = Vec::new();
                        match accounts { Ok(snapshot) => page.accounts = snapshot.accounts, Err(error) => errors.push(format!("Shared accounts: {error}")) }
                        if let Some(environments) = environments {
                            match environments { Ok(snapshot) => page.environments = snapshot.environments, Err(error) => errors.push(format!("Sandboxes: {error}")) }
                        }
                        if let Some(models) = models {
                            match models { Ok(models) => page.models = models.into_iter().filter(|model| AgentRoute::from_omp_model(&model.id).is_some()).collect(), Err(error) => errors.push(format!("Models: {error}")) }
                        }
                        page.error = (!errors.is_empty()).then(|| errors.join("\n"));
                    }
                    Err(error) => page.error = Some(error),
                }
                if page.view == CrewView::Sandboxes && !refresh_environments { page.refresh(cx); }
                cx.notify();
            }).ok();
        }).detach();
        cx.notify();
    }

    fn request(&mut self, method: &'static str, params: Value, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if method == "scaffold.control"
            && self
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome.status == "unknown" || outcome.status == "pending")
        {
            self.error = Some("The previous command has an unknown outcome. Reconcile it before issuing another command; never recreate a sandbox to retry.".into());
            cx.notify();
            return;
        }
        let Some(status) = &self.status else {
            self.error =
                Some("Refresh the verified Crew scope before issuing an operation.".into());
            cx.notify();
            return;
        };
        let recovery_key = json!([status.principal_id, status.scope]).to_string();
        if method == "scaffold.control" {
            if let Some(pending) = super::current(cx).crew_recovery_commands.get(&recovery_key) {
                let next = self
                    .outcome
                    .as_ref()
                    .filter(|outcome| &outcome.command_id == pending)
                    .and_then(attachment_after_create);
                if !next.is_some_and(|(id, _)| {
                    params.get("commandId").and_then(Value::as_str) == Some(id.as_str())
                }) {
                    self.outcome = Some(Outcome {
                        command_id: pending.clone(),
                        status: "unknown".into(),
                        result: None,
                        error: None,
                    });
                    self.error = Some(
                        "Reconcile the pending Crew operation before issuing another command."
                            .into(),
                    );
                    cx.notify();
                    return;
                }
            }
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine unavailable. No action was sent.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.error = None;
        self.notice = None;
        let command_id = params
            .get("commandId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(id) = command_id.as_ref() {
            super::update(super::SavePolicy::Immediate, cx, |settings| {
                settings
                    .crew_recovery_commands
                    .insert(recovery_key.clone(), id.clone());
            });
            self.outcome = Some(Outcome {
                command_id: id.clone(),
                status: "pending".into(),
                result: None,
                error: None,
            });
        }
        cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(value) if command_id.is_some() => {
                        match serde_json::from_value::<Outcome>(value) {
                            Ok(outcome) => {
                                if let Some((command_id, control)) =
                                    attachment_after_create(&outcome)
                                {
                                    page.outcome = Some(outcome);
                                    page.request(
                                        "scaffold.control",
                                        json!({"commandId": command_id, "control": control}),
                                        cx,
                                    );
                                    return;
                                }
                                if let Some(result) = &outcome.result {
                                    page.environments.retain(|environment| {
                                        environment.scope != result.environment.scope
                                    });
                                    page.environments.push(result.environment.clone());
                                    if attached_session(&outcome).is_some() {
                                        if let Some(link) =
                                            crate::links::crew_scope_link(&result.environment.scope)
                                        {
                                            page.state.update(cx, |state, cx| {
                                                state.open_deep_link(&link, cx)
                                            });
                                        }
                                    }
                                }
                                if matches!(outcome.status.as_str(), "confirmed" | "rejected") {
                                    super::update(super::SavePolicy::Immediate, cx, |settings| {
                                        if settings.crew_recovery_commands.get(&recovery_key)
                                            == Some(&outcome.command_id)
                                        {
                                            settings.crew_recovery_commands.remove(&recovery_key);
                                        }
                                    });
                                }
                                let rejected = outcome.status == "rejected";
                                page.outcome = Some(outcome);
                                if rejected {
                                    page.refresh(cx);
                                }
                            }
                            Err(error) => {
                                page.mark_unknown(error.to_string());
                            }
                        }
                    }
                    Ok(_) => {
                        page.notice = Some("Operation confirmed.".into());
                        page.refresh(cx);
                    }
                    Err(error) if command_id.is_some() => page.mark_unknown(error.to_string()),
                    Err(error) => page.error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn mark_unknown(&mut self, error: String) {
        if let Some(outcome) = self.outcome.as_mut() {
            outcome.status = "unknown".into();
            outcome.error = Some(error);
        }
    }

    fn control(&mut self, control: ScaffoldEnvironmentControl, cx: &mut Context<Self>) {
        self.request(
            "scaffold.control",
            json!({"commandId": uuid::Uuid::new_v4().to_string(), "control": control}),
            cx,
        );
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(status) = &self.status else {
            return;
        };
        let Some(route) = self.agent_route() else {
            self.error = Some("Choose a managed model before creating a sandbox.".into());
            cx.notify();
            return;
        };
        let mut scope = status.scope.clone();
        scope.session_id = Some(uuid::Uuid::new_v4().to_string());
        let description = format!(
            "Create a new Scaffold sandbox using {} and the {} database? Source and region use deployment defaults.",
            route.omp_model(),
            self.database.as_str()
        );
        let control = ScaffoldEnvironmentControl::Create {
            scope,
            name: None,
            source_ref: None,
            region: None,
            database_environment: self.database,
            agent_route: route,
        };
        self.confirmation = Some((
            "scaffold.control",
            json!({"commandId": format!("{}:create", uuid::Uuid::new_v4()), "control": control}),
            description,
        ));
        cx.notify();
    }

    pub(crate) fn view(&self) -> CrewView {
        self.view
    }

    pub(crate) fn set_view(
        &mut self,
        view: CrewView,
        database: Option<ScaffoldDatabaseEnvironment>,
        cx: &mut Context<Self>,
    ) {
        if let Some(database) = database {
            self.database = database;
            self.confirmation = None;
            cx.notify();
        }
        if self.view != view {
            self.view = view;
            self.confirmation = None;
            self.error = None;
            self.notice = None;
            if view != CrewView::Annotations {
                self.refresh(cx);
            }
            cx.notify();
        }
    }

    fn agent_route(&self) -> Option<AgentRoute> {
        let selected = self.model.as_deref()?;
        if !self.models.iter().any(|model| model.id == selected) {
            return None;
        }
        let route = AgentRoute::from_omp_model(selected)?;
        Some(match &self.account {
            Some(account) => {
                let provider = match route.provider {
                    AgentProvider::Anthropic => "anthropic",
                    AgentProvider::OpenAi => "openai",
                };
                if !self.accounts.iter().any(|candidate| {
                    candidate.id == *account
                        && candidate.provider.eq_ignore_ascii_case(provider)
                        && matches!(candidate.status.as_str(), "active" | "connected" | "ready")
                }) {
                    return None;
                }
                AgentRoute::pinned(route.provider, route.model, account.clone())
            }
            None => route,
        })
    }

    pub fn capture_annotation_selection(&mut self, cx: &mut Context<Self>) {
        self.annotations
            .update(cx, |page, cx| page.capture_selection(cx));
    }

    pub(super) fn action(
        theme: &Theme,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
    ) -> gpui::Stateful<gpui::Div> {
        let label = label.into();
        let accent = theme.accent;
        widgets::ghost_action(theme)
            .id(id.into())
            .role(gpui::Role::Button)
            .aria_label(label.clone())
            .tab_index(0)
            .focus_visible(move |style| style.border_2().border_color(accent))
            .child(label)
    }

    pub(super) fn field(
        theme: &Theme,
        label: &'static str,
        input: &Entity<ComposerInput>,
    ) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(widgets::row_title(theme, label.to_string()))
            .child(
                div()
                    .border_1()
                    .border_color(theme.text_muted.opacity(0.3))
                    .rounded(px(6.0))
                    .p(px(8.0))
                    .child(input.clone()),
            )
    }
}

impl Render for CrewAccounts {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let title = match self.view {
            CrewView::Accounts => "Accounts",
            CrewView::Sandboxes => "Sandboxes",
            CrewView::Annotations => "Annotations",
        };
        let mut body = div().flex().flex_col().gap(px(16.0)).child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(widgets::page_header(
                    &theme,
                    title,
                    (self.view == CrewView::Accounts).then_some(self.accounts.len()),
                ))
                .when(self.view != CrewView::Annotations, |el| {
                    el.child(
                        Self::action(
                            &theme,
                            "crew-refresh",
                            if self.busy {
                                "Refreshing…"
                            } else {
                                "Refresh"
                            },
                        )
                        .on_click(cx.listener(|page, _, _, cx| page.refresh(cx))),
                    )
                }),
        );
        if !matches!(
            self.state.read(cx).auth,
            Some(zeron_proto::AuthState::SignedIn { .. })
        ) {
            return body
                .child(widgets::page_subtitle(
                    &theme,
                    "Sign in using the account menu to continue.",
                ))
                .into_any_element();
        }
        if self.view == CrewView::Annotations {
            return body.child(self.annotations.clone()).into_any_element();
        }
        if let Some(error) = &self.error {
            body = body.child(widgets::error_strip(&theme, error.clone()));
        }
        if let Some(notice) = &self.notice {
            body = body.child(widgets::page_subtitle(&theme, notice.clone()));
        }
        if let Some((_, _, description)) = &self.confirmation {
            let card = crate::popover::dialog_card(&theme)
                .id("crew-confirmation-card")
                .role(gpui::Role::Dialog)
                .aria_label("Confirm operation")
                .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        page.confirmation = None;
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .child(widgets::row_title(&theme, "Confirm operation"))
                .child(widgets::page_subtitle(&theme, description.clone()))
                .child(
                    Self::action(&theme, "crew-confirm", "Confirm")
                        .tab_index(if self.busy { -1 } else { 0 })
                        .when(self.busy, |el| {
                            el.opacity(0.5)
                                .aria_description("Unavailable while a request is pending")
                        })
                        .on_click(cx.listener(|page, _, _, cx| {
                            if page.busy {
                                return;
                            }
                            if let Some((method, params, _)) = page.confirmation.take() {
                                page.request(method, params, cx);
                            }
                        })),
                )
                .child(
                    Self::action(&theme, "crew-cancel", "Cancel").on_click(cx.listener(
                        |page, _, _, cx| {
                            page.confirmation = None;
                            cx.notify();
                        },
                    )),
                );
            body = body.child(crate::popover::modal(
                "crew-confirmation",
                window.viewport_size(),
                card.into_any_element(),
            ));
        }
        let Some(status) = self.status.clone() else {
            return body.into_any_element();
        };
        if self.view == CrewView::Accounts {
            let now = chrono::Utc::now();
            for (provider, title, harness) in [
                ("anthropic", "Anthropic", zeron_proto::HarnessId::ClaudeCode),
                ("openai", "OpenAI", zeron_proto::HarnessId::Codex),
            ] {
                let mut accounts = self
                    .accounts
                    .iter()
                    .filter(|account| account.provider.eq_ignore_ascii_case(provider))
                    .peekable();
                let header = div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                crate::icons::icon(if harness == zeron_proto::HarnessId::Codex {
                                    crate::icons::OPENAI_MARK
                                } else {
                                    crate::icons::CLAUDE_MARK
                                })
                                .size(px(20.0))
                                .text_color(theme.text_muted),
                            )
                            .child(widgets::row_title(&theme, title)),
                    )
                    .when(status.role == "local_controller", |el| {
                        el.child(
                            Self::action(&theme, format!("crew-add-{provider}"), "Add account")
                                .on_click(
                                    cx.listener(move |_, _, _, cx| cx.emit(AddAccount(harness))),
                                ),
                        )
                    });
                let mut card = widgets::section_card(&theme).flex().flex_col();
                if accounts.peek().is_none() {
                    card = card.child(
                        div()
                            .p(px(16.0))
                            .child(widgets::page_subtitle(&theme, "No accounts yet.")),
                    );
                }
                for account in accounts {
                    let label = account
                        .email
                        .clone()
                        .or(account.display_name.clone())
                        .unwrap_or_else(|| "Shared account".into());
                    let healthy =
                        matches!(account.status.as_str(), "connected" | "active" | "ready");
                    let row = div()
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .p(px(16.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_none()
                                .size(px(32.0))
                                .rounded_full()
                                .bg(theme.wash(0.09))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    label
                                        .chars()
                                        .next()
                                        .map(|c| c.to_uppercase().to_string())
                                        .unwrap_or_else(|| "?".into()),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .gap(px(12.0))
                                        .child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .child(widgets::row_title(&theme, label)),
                                        )
                                        .when_some(account.plan_label.clone(), |row, plan| {
                                            row.child(widgets::badge(&theme, plan))
                                        }),
                                )
                                .when(!healthy, |el| {
                                    el.child(widgets::page_subtitle(
                                        &theme,
                                        match account.status.as_str() {
                                            "disabled" => "Disabled",
                                            "revoked" => "Revoked",
                                            _ => "Needs attention",
                                        },
                                    ))
                                })
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(5.0))
                                        .children(account.usage_windows.iter().map(|usage| {
                                            div()
                                                .flex()
                                                .items_center()
                                                .flex_wrap()
                                                .gap(px(8.0))
                                                .child(super::accounts::render_usage_meter_width(
                                                    usage, &theme, 230.0,
                                                ))
                                                .when_some(
                                                    super::accounts::format_reset(
                                                        usage.resets_at,
                                                        now,
                                                    ),
                                                    |el, reset| {
                                                        el.child(widgets::page_subtitle(
                                                            &theme, reset,
                                                        ))
                                                    },
                                                )
                                        }))
                                        .when(account.usage_windows.is_empty(), |el| {
                                            el.child(widgets::page_subtitle(
                                                &theme,
                                                "Usage unavailable",
                                            ))
                                        }),
                                ),
                        );
                    card = card.child(row);
                }
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(header)
                        .child(card),
                );
            }
            return body.into_any_element();
        }
        if let Some(outcome) = &self.outcome {
            body = body.child(
                widgets::section_card(&theme)
                    .p(px(12.0))
                    .child(
                        div()
                            .id("crew-command-details")
                            .tooltip(widgets::text_tooltip(format!(
                                "Command {}",
                                outcome.command_id
                            )))
                            .child(widgets::row_title(
                                &theme,
                                format!("Last operation: {}", outcome.status),
                            )),
                    )
                    .child(widgets::page_subtitle(
                        &theme,
                        outcome.error.clone().unwrap_or_else(|| {
                            "State is the authoritative inspected result, not HTTP acceptance."
                                .into()
                        }),
                    ))
                    .child(
                        Self::action(&theme, "crew-reconcile", "Check status").on_click(
                            cx.listener(|page, _, _, cx| {
                                if let Some(outcome) = &page.outcome {
                                    let id = outcome.command_id.clone();
                                    page.request(
                                        "scaffold.reconcile",
                                        json!({"commandId": id}),
                                        cx,
                                    );
                                }
                            }),
                        ),
                    ),
            );
        }
        if status.role == "local_controller" {
            body = body.child(widgets::row_title(&theme, "New sandbox"));
            let model = self.model_picker.clone();
            body = body.child(
                widgets::section_card(&theme)
                    .p(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(model)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(12.0))
                            .child(
                                Self::action(&theme, "crew-create", "Create sandbox…")
                                    .on_click(cx.listener(|page, _, _, cx| page.create(cx))),
                            )
                            .child(
                                Self::action(
                                    &theme,
                                    "crew-sandbox-options",
                                    if self.sandbox_options {
                                        "Hide options"
                                    } else {
                                        "Account & database…"
                                    },
                                )
                                .aria_description("Choose a shared account or database snapshot")
                                .on_click(cx.listener(
                                    |page, _, _, cx| {
                                        page.sandbox_options = !page.sandbox_options;
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .when(self.sandbox_options, |el| {
                        let provider = self.agent_route().map(|route| route.provider);
                        let candidates: Vec<_> = self
                            .accounts
                            .iter()
                            .filter(|account| {
                                matches!(account.status.as_str(), "active" | "connected" | "ready")
                                    && match provider {
                                        Some(AgentProvider::Anthropic) => {
                                            account.provider.eq_ignore_ascii_case("anthropic")
                                        }
                                        Some(AgentProvider::OpenAi) => {
                                            account.provider.eq_ignore_ascii_case("openai")
                                        }
                                        None => false,
                                    }
                            })
                            .collect();
                        let selected = candidates
                            .iter()
                            .position(|account| Some(&account.id) == self.account.as_ref())
                            .map_or(0, |ix| ix + 1);
                        let ids: Vec<_> = candidates
                            .iter()
                            .map(|account| account.id.clone())
                            .collect();
                        let options =
                            std::iter::once(widgets::SelectOption::new("Automatic · shared pool"))
                                .chain(candidates.iter().map(|account| {
                                    widgets::SelectOption::new(
                                        account
                                            .email
                                            .clone()
                                            .or(account.display_name.clone())
                                            .unwrap_or_else(|| "Shared account".into()),
                                    )
                                }))
                                .collect::<Vec<_>>();
                        let account = widgets::select(
                            "crew-account",
                            "Shared account",
                            &theme,
                            |page: &mut Self| &mut page.account_select,
                        )
                        .options(options, selected)
                        .on_select(move |page, ix, _, cx| {
                            page.account = ix.checked_sub(1).and_then(|ix| ids.get(ix).cloned());
                            cx.notify();
                        })
                        .render(&self.account_select, cx);
                        let database = widgets::select(
                            "crew-database",
                            "Database environment",
                            &theme,
                            |page: &mut Self| &mut page.database_select,
                        )
                        .options(
                            ["Local", "Staging snapshot", "Production snapshot"]
                                .into_iter()
                                .map(widgets::SelectOption::new),
                            match self.database {
                                ScaffoldDatabaseEnvironment::Local => 0,
                                ScaffoldDatabaseEnvironment::StagingSnapshot => 1,
                                ScaffoldDatabaseEnvironment::ProductionSnapshot => 2,
                            },
                        )
                        .on_select(|page, ix, _, cx| {
                            page.database = match ix {
                                1 => ScaffoldDatabaseEnvironment::StagingSnapshot,
                                2 => ScaffoldDatabaseEnvironment::ProductionSnapshot,
                                _ => ScaffoldDatabaseEnvironment::Local,
                            };
                            cx.notify();
                        })
                        .render(&self.database_select, cx);
                        el.child(account).child(database)
                    }),
            );
        } else {
            body = body.child(widgets::page_subtitle(&theme, "Browse-only on this host. Open Sandboxes on your Crew controller to create or manage environments."));
        }
        body = body.child(widgets::row_title(&theme, "Use an existing sandbox"));
        if self.environments.is_empty() {
            body = body.child(widgets::page_subtitle(&theme, "No sandboxes yet. Create one above to run Crew in Scaffold, or refresh to find an existing environment."));
        }
        for environment in self.environments.clone() {
            let SessionEnvironmentSource::Scaffold {
                sandbox_id,
                lifecycle,
                lifecycle_epoch,
                ..
            } = &environment.source
            else {
                continue;
            };
            let name = environment.name.clone().unwrap_or_else(|| {
                format!(
                    "Sandbox · {}",
                    environment.source_ref.as_deref().unwrap_or("Scaffold")
                )
            });
            let stopped = matches!(lifecycle, zeron_proto::ScaffoldLifecycle::Stopped);
            let terminal = stopped || *lifecycle == zeron_proto::ScaffoldLifecycle::Failed;
            let expected_epoch = *lifecycle_epoch;
            let mut row = widgets::section_card(&theme)
                .p(px(12.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "crew-sandbox-details-{sandbox_id}"
                        )))
                        .min_w_0()
                        .truncate()
                        .tooltip(widgets::text_tooltip(format!(
                            "Sandbox {sandbox_id}\nOwner {}\nProject {}\nDeployment {}\nSession {}",
                            environment.owner_principal,
                            environment.scope.project_id,
                            environment.scope.deployment_id.as_deref().unwrap_or("none"),
                            environment
                                .scope
                                .session_id
                                .as_deref()
                                .unwrap_or("unassigned")
                        )))
                        .child(widgets::row_title(&theme, name.clone())),
                )
                .child(widgets::page_subtitle(
                    &theme,
                    format!(
                        "{} · {}",
                        lifecycle_label(*lifecycle),
                        environment
                            .source_ref
                            .as_deref()
                            .unwrap_or("Source unavailable")
                    ),
                ));
            if let Some(link) = crate::links::crew_scope_link(&environment.scope) {
                row = row.child(
                    Self::action(&theme, format!("crew-open-{sandbox_id}"), "Open history")
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.state
                                .update(cx, |state, cx| state.open_deep_link(&link, cx))
                        })),
                );
            }
            let mut controls = div().flex().flex_wrap().gap(px(8.0));
            let expanded = self.managed_sandbox.as_ref() == Some(sandbox_id);
            let mut has_maintenance = false;
            for operation in [
                "Use sandbox…",
                "Start new epoch…",
                "Resume…",
                "Pause…",
                "Apply model/account…",
                "Inspect",
                "Stop sandbox…",
            ] {
                if status.role != "local_controller" && operation != "Inspect" {
                    continue;
                }
                if stopped && !matches!(operation, "Inspect" | "Start new epoch…") {
                    continue;
                }
                if *lifecycle == zeron_proto::ScaffoldLifecycle::Failed
                    && !matches!(operation, "Inspect" | "Stop sandbox…" | "Start new epoch…")
                {
                    continue;
                }
                if operation == "Start new epoch…"
                    && (!terminal || expected_epoch.is_none_or(|epoch| epoch == 0))
                {
                    continue;
                }
                if operation == "Pause…" && *lifecycle == zeron_proto::ScaffoldLifecycle::Paused {
                    continue;
                }
                if operation == "Resume…" && *lifecycle != zeron_proto::ScaffoldLifecycle::Paused
                {
                    continue;
                }
                if operation != "Use sandbox…"
                    && !(operation == "Inspect"
                        && *lifecycle == zeron_proto::ScaffoldLifecycle::Failed)
                {
                    has_maintenance = true;
                    if !expanded {
                        continue;
                    }
                }
                let sandbox = sandbox_id.clone();
                let scope = environment.scope.clone();
                let label = name.clone();
                controls = controls.child(Self::action(&theme, format!("crew-{operation}-{sandbox}"), operation).on_click(cx.listener(move |page, _, _, cx| {
                    let control = match operation {
                        "Apply model/account…" => {
                            let Some(agent_route) = page.agent_route() else {
                                page.error = Some("Choose a managed model in the new-sandbox form above first.".into()); cx.notify(); return;
                            };
                            ScaffoldEnvironmentControl::UpdateAgentRoute { sandbox_id: sandbox.clone(), scope: scope.clone(), agent_route }
                        }
                        "Inspect" => ScaffoldEnvironmentControl::Inspect { sandbox_id: sandbox.clone(), scope: scope.clone() },
                        "Pause…" => ScaffoldEnvironmentControl::Pause { sandbox_id: sandbox.clone(), scope: scope.clone() },
                        "Resume…" => ScaffoldEnvironmentControl::Resume { sandbox_id: sandbox.clone(), scope: scope.clone() },
                        "Start new epoch…" => ScaffoldEnvironmentControl::Start { sandbox_id: sandbox.clone(), scope: scope.clone(), expected_lifecycle_epoch: expected_epoch.expect("eligible observed epoch") },
                        "Stop sandbox…" => ScaffoldEnvironmentControl::Stop { sandbox_id: sandbox.clone(), scope: scope.clone() },
                        _ => ScaffoldEnvironmentControl::Attach { sandbox_id: sandbox.clone(), scope: scope.clone() },
                    };
                    if operation == "Inspect" { page.control(control, cx); }
                    else {
                        let warning = if operation == "Stop sandbox…" { "Stop permanently ends this host epoch. The same logical session may later explicitly start a fresh epoch; its old native process is never resumed. This is NOT the composer Stop button." } else if operation == "Start new epoch…" { "Start a fresh verified host epoch for this same session. Original history and its receipt stay immutable; only a new user request may begin a new native conversation. This does not resume the destroyed native process." } else if operation == "Use sandbox…" { "Attach to this same sandbox; if paused, it will resume. This does not create another sandbox." } else { "Apply this operation to the selected sandbox?" };
                        page.confirmation = Some(("scaffold.control", json!({"commandId": uuid::Uuid::new_v4().to_string(), "control": control}), format!("{operation} {label}\n{warning}"))); cx.notify();
                    }
                })));
            }
            if has_maintenance {
                let sandbox = sandbox_id.clone();
                controls = controls.child(
                    Self::action(
                        &theme,
                        format!("crew-manage-{sandbox_id}"),
                        if expanded { "Less" } else { "More…" },
                    )
                    .aria_description(if expanded {
                        "Hide sandbox maintenance actions"
                    } else {
                        "Show sandbox maintenance actions"
                    })
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.managed_sandbox = if page.managed_sandbox.as_ref() == Some(&sandbox) {
                            None
                        } else {
                            Some(sandbox.clone())
                        };
                        cx.notify();
                    })),
                );
            }
            body = body.child(row.child(controls));
        }
        body.into_any_element()
    }
}

pub(crate) fn lifecycle_label(lifecycle: zeron_proto::ScaffoldLifecycle) -> &'static str {
    use zeron_proto::ScaffoldLifecycle::*;
    match lifecycle {
        Creating => "Creating",
        RestoringSnapshot => "Restoring snapshot",
        Starting => "Starting",
        Ready => "Ready",
        AgentRunning => "Agent running",
        Paused => "Paused",
        Resuming => "Resuming",
        Stopped => "Stopped",
        Failed => "Needs attention",
    }
}
