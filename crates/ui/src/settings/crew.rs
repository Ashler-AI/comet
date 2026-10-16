//! Crew's authority-owned accounts and scoped environments, within Accounts settings.
use gpui::{div, prelude::*, px, Context, Entity, SharedString, Subscription, Window};
use serde::Deserialize;
use serde_json::{json, Value};
use zeron_proto::{AgentProvider, AgentRoute, CollaborationScope, ScaffoldDatabaseEnvironment,
    ScaffoldEnvironmentControl, ScaffoldEnvironmentControlResult, ScaffoldEnvironmentSnapshot,
    SessionEnvironmentSource};
use crate::{composer::ComposerInput, state::AppState, theme::Theme};
use super::widgets;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CrewStatus {
    scope: CollaborationScope,
    principal_id: String,
    managed: bool,
    role: String,
    runtime_version: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SharedAccount {
    id: String,
    provider: String,
    email: Option<String>,
    display_name: Option<String>,
    status: String,
    #[serde(default)]
    usage_windows: Vec<zeron_proto::AgentUsageWindow>,
}

#[derive(Deserialize)]
struct Accounts { accounts: Vec<SharedAccount> }

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Outcome {
    command_id: String,
    status: String,
    result: Option<ScaffoldEnvironmentControlResult>,
    error: Option<String>,
}

pub struct CrewAccounts {
    state: Entity<AppState>,
    annotations: Entity<super::annotations::Annotations>,
    status: Option<CrewStatus>,
    accounts: Vec<SharedAccount>,
    local_accounts: Vec<zeron_proto::AgentAccount>,
    environments: Vec<zeron_proto::SessionEnvironment>,
    outcome: Option<Outcome>,
    update_agents_after_crew: Option<bool>,
    busy: bool,
    error: Option<String>,
    notice: Option<String>,
    confirmation: Option<(&'static str, Value, String)>,
    source: Entity<ComposerInput>,
    region: Entity<ComposerInput>,
    model: Entity<ComposerInput>,
    account: Entity<ComposerInput>,
    command: Entity<ComposerInput>,
    database: ScaffoldDatabaseEnvironment,
    database_select: widgets::SelectState,
    provider: AgentProvider,
    provider_select: widgets::SelectState,
    _observe: Subscription,
}

impl CrewAccounts {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            annotations: cx.new(|cx| super::annotations::Annotations::new(state.clone(), cx)),
            state, status: None, accounts: vec![], environments: vec![], outcome: None,
            local_accounts: Vec::new(),
            update_agents_after_crew: None,
            busy: false, error: None, notice: None, confirmation: None,
            source: cx.new(|cx| ComposerInput::new("Source ref (branch or commit)", cx)),
            region: cx.new(|cx| ComposerInput::new("Region (empty uses deployment default)", cx)),
            model: cx.new(|cx| ComposerInput::new("Model ID", cx)),
            account: cx.new(|cx| ComposerInput::new("Account ID (empty: authority selects)", cx)),
            command: cx.new(|cx| ComposerInput::new("Command ID to recover after restart", cx)),
            database: ScaffoldDatabaseEnvironment::Local,
            database_select: widgets::SelectState::default(),
            provider: AgentProvider::OpenAi, provider_select: widgets::SelectState::default(),
            _observe: observe,
        };
        if page.state.read(cx).managed_inference { page.refresh(cx); }
        page
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.busy { return; }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine unavailable. Reconnect, then refresh.".into()); cx.notify(); return;
        };
        self.busy = true;
        self.error = None;
        cx.spawn(async move |this, cx| {
            let result = async {
                let status: CrewStatus = serde_json::from_value(engine.client().call("crew.status", json!({})).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
                let accounts = engine.client().call("crew.accounts.list", json!({})).await.map_err(|e| e.to_string())
                    .and_then(|value| serde_json::from_value::<Accounts>(value).map_err(|e| e.to_string()));
                let environments = engine.client().call("scaffold.refresh", json!(status.scope)).await.map_err(|e| e.to_string())
                    .and_then(|value| serde_json::from_value::<ScaffoldEnvironmentSnapshot>(value).map_err(|e| e.to_string()));
                let local = engine.client().call(zeron_rpc::methods::LIST_AGENT_ACCOUNTS, json!({"forceUsage": false})).await.map_err(|e| e.to_string())
                    .and_then(|value| serde_json::from_value::<zeron_proto::AgentAccountsSnapshot>(value).map_err(|e| e.to_string()));
                let updates = engine.client().call("GetHarnessAutoUpdate", json!({})).await.map_err(|e| e.to_string())
                    .and_then(|value| value.get("enabled").and_then(Value::as_bool).ok_or_else(|| "Missing post-update preference".to_string()));
                Ok::<_, String>((status, accounts, environments, local, updates))
            }.await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok((status, accounts, environments, local, updates)) => {
                        let recovery_key = json!([status.principal_id, status.scope]).to_string();
                        if page.outcome.is_none() {
                            if let Some(id) = super::current(cx).crew_recovery_commands.get(&recovery_key).cloned() {
                                page.command.update(cx, |input, cx| input.set_text(id.clone(), cx));
                                page.outcome = Some(Outcome { command_id: id, status: "unknown".into(), result: None, error: Some("Recovered command; reconcile before issuing another operation.".into()) });
                            }
                        }
                        page.status = Some(status);
                        let mut errors = Vec::new();
                        match accounts { Ok(snapshot) => page.accounts = snapshot.accounts, Err(error) => errors.push(format!("Shared accounts: {error}")) }
                        match environments { Ok(snapshot) => page.environments = snapshot.environments, Err(error) => errors.push(format!("Environments: {error}")) }
                        match local { Ok(snapshot) => page.local_accounts = snapshot.accounts, Err(error) => errors.push(format!("Local import candidates: {error}")) }
                        match updates { Ok(enabled) => page.update_agents_after_crew = Some(enabled), Err(error) => errors.push(format!("Agent update preference: {error}")) }
                        page.error = (!errors.is_empty()).then(|| errors.join("\n"));
                    }
                    Err(error) => page.error = Some(error),
                }
                cx.notify();
            }).ok();
        }).detach();
        cx.notify();
    }

    fn request(&mut self, method: &'static str, params: Value, cx: &mut Context<Self>) {
        if self.busy { return; }
        if method == "scaffold.control" && self.outcome.as_ref().is_some_and(|outcome| outcome.status == "unknown" || outcome.status == "pending") {
            self.error = Some("The previous command has an unknown outcome. Reconcile it before issuing another command; never recreate a sandbox to retry.".into());
            cx.notify(); return;
        }
        let Some(status) = &self.status else {
            self.error = Some("Refresh the verified Crew scope before issuing an operation.".into()); cx.notify(); return;
        };
        let recovery_key = json!([status.principal_id, status.scope]).to_string();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine unavailable. No action was sent.".into()); cx.notify(); return;
        };
        self.busy = true; self.error = None; self.notice = None;
        let command_id = params.get("commandId").and_then(Value::as_str).map(str::to_owned);
        if let Some(id) = command_id.as_ref() {
            self.command.update(cx, |input, cx| input.set_text(id.clone(), cx));
            super::update(super::SavePolicy::Immediate, cx, |settings| { settings.crew_recovery_commands.insert(recovery_key.clone(), id.clone()); });
            self.outcome = Some(Outcome { command_id: id.clone(), status: "pending".into(), result: None, error: None });
        }
        cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(value) if command_id.is_some() => match serde_json::from_value::<Outcome>(value) {
                        Ok(outcome) => {
                            if let Some(result) = &outcome.result {
                                page.environments.retain(|environment| environment.scope != result.environment.scope);
                                page.environments.push(result.environment.clone());
                            }
                            if matches!(outcome.status.as_str(), "confirmed" | "rejected") {
                                super::update(super::SavePolicy::Immediate, cx, |settings| { settings.crew_recovery_commands.remove(&recovery_key); });
                            }
                            page.outcome = Some(outcome);
                        }
                        Err(error) => { page.mark_unknown(error.to_string()); }
                    },
                    Ok(_) => { page.notice = Some("Operation confirmed.".into()); page.refresh(cx); }
                    Err(error) if command_id.is_some() => page.mark_unknown(error.to_string()),
                    Err(error) => page.error = Some(error.to_string()),
                }
                cx.notify();
            }).ok();
        }).detach();
        cx.notify();
    }

    fn mark_unknown(&mut self, error: String) {
        if let Some(outcome) = self.outcome.as_mut() {
            outcome.status = "unknown".into(); outcome.error = Some(error);
        }
    }

    fn control(&mut self, control: ScaffoldEnvironmentControl, cx: &mut Context<Self>) {
        self.request("scaffold.control", json!({"commandId": uuid::Uuid::new_v4().to_string(), "control": control}), cx);
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        let Some(status) = &self.status else { return; };
        let model = self.model.read(cx).text().trim().to_owned();
        let source = self.source.read(cx).text().trim().to_owned();
        if model.is_empty() || source.is_empty() {
            self.error = Some("Enter a source ref and model before creating a sandbox.".into()); cx.notify(); return;
        }
        let account = self.account.read(cx).text().trim().to_owned();
        let route = if account.is_empty() { AgentRoute::automatic(self.provider, model) }
            else { AgentRoute::pinned(self.provider, model, account) };
        let region = self.region.read(cx).text().trim().to_owned();
        let mut scope = status.scope.clone();
        scope.session_id = Some(uuid::Uuid::new_v4().to_string());
        let control = ScaffoldEnvironmentControl::Create {
            scope, name: None, source_ref: Some(source),
            region: (!region.is_empty()).then_some(region), database_environment: self.database,
            agent_route: route,
        };
        self.confirmation = Some(("scaffold.control", json!({"commandId": uuid::Uuid::new_v4().to_string(), "control": control}),
            format!("Create a new sandbox in {} / {:?} using runtime {} and database {}? This is not an attachment to an existing session.\n{}", status.scope.project_id, status.scope.deployment_id, status.runtime_version, self.database.as_str(), json!(control))));
        cx.notify();
    }

    pub fn capture_annotation_selection(&mut self, cx: &mut Context<Self>) {
        self.annotations.update(cx, |page, cx| page.capture_selection(cx));
    }

    pub(super) fn action(theme: &Theme, id: impl Into<SharedString>, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
        let label = label.into();
        let accent = theme.accent;
        widgets::ghost_action(theme).id(id.into()).role(gpui::Role::Button).aria_label(label.clone())
            .tab_index(0).focus_visible(move |style| style.border_2().border_color(accent)).child(label)
    }

    pub(super) fn field(theme: &Theme, label: &'static str, input: &Entity<ComposerInput>) -> gpui::Div {
        div().flex().flex_col().gap(px(4.0)).child(widgets::row_title(theme, label.to_string()))
            .child(div().border_1().border_color(theme.text_muted.opacity(0.3)).rounded(px(6.0)).p(px(8.0)).child(input.clone()))
    }
}

impl Render for CrewAccounts {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let signed_in = matches!(self.state.read(cx).auth, Some(zeron_proto::AuthState::SignedIn { .. }));
        let mut body = div().flex().flex_col().gap(px(12.0))
            .child(widgets::page_header(&theme, "Crew Canary · shared accounts and environments", None))
            .child(widgets::page_subtitle(&theme, "Agent Auth selects shared accounts on the server. Provider secrets never enter this page. OMP, Claude, Codex and session titles use managed inference. Other harnesses and realtime voice are unavailable in managed mode."))
            .child(Self::action(&theme, "crew-refresh", if self.busy { "Working…" } else { "Refresh accounts and environments" })
                .on_click(cx.listener(|page, _, _, cx| page.refresh(cx))));
        if !signed_in {
            return body.child(widgets::page_subtitle(&theme, "Sign in to Crew using the account menu. Signing out does not enable local-provider fallback.")).into_any_element();
        }
        if let Some(error) = &self.error { body = body.child(widgets::error_strip(&theme, error.clone())); }
        if let Some(notice) = &self.notice { body = body.child(widgets::page_subtitle(&theme, notice.clone())); }
        body = body.child(self.annotations.clone());
        let Some(status) = self.status.clone() else { return body.into_any_element(); };
        body = body.child(widgets::page_subtitle(&theme, format!("Verified owner: {} · project: {} · deployment: {} · {} · runtime {}",
            status.principal_id, status.scope.project_id, status.scope.deployment_id.as_deref().unwrap_or("none"),
            if status.managed { "Agent Auth managed" } else { "Explicit local-provider mode" }, status.runtime_version)));
        if status.role != "local_controller" {
            return body.child(widgets::page_subtitle(&theme, "This sandbox host cannot import accounts or create other sandboxes. Open these settings on your controller.")).into_any_element();
        }
        if let Some(enabled) = self.update_agents_after_crew {
            body = body.child(widgets::section_card(&theme).p(px(12.0))
                .child(widgets::row_title(&theme, "Update agents after Crew updates"))
                .child(widgets::page_subtitle(&theme, "Preference preserved for signed Crew delivery. Manual Crew Canary packages do not automatically update Crew or agents."))
                .child(widgets::toggle_switch(&theme, enabled, "crew-post-update-agents")
                    .id("crew-post-update-agents").role(gpui::Role::Switch)
                    .aria_label("Update agents after Crew updates")
                    .aria_toggled(if enabled { gpui::Toggled::True } else { gpui::Toggled::False })
                    .tab_index(if self.busy { -1 } else { 0 })
                    .when(self.busy, |el| el.opacity(0.5).aria_description("Unavailable while a request is pending"))
                    .focus_visible(|style| style.border_2().border_color(theme.accent))
                    .on_click(cx.listener(move |page, _, _, cx| page.request("SetHarnessAutoUpdate", json!({"enabled": !enabled}), cx)))));
        }
        for account in self.accounts.clone() {
            let id = account.id.clone();
            let description = format!("{} · {} · {} · {}", account.display_name.or(account.email).unwrap_or_else(|| id.clone()), account.provider, account.status, id);
            let mut row = widgets::section_card(&theme).p(px(12.0)).child(widgets::row_title(&theme, description));
            for usage in &account.usage_windows { row = row.child(super::accounts::render_usage_meter(usage, &theme)); }
            let revoke_id = id.clone();
            let provider = serde_json::from_value::<AgentProvider>(json!(account.provider)).ok();
            row = row.child(Self::action(&theme, format!("crew-select-{id}"), "Use for new sandbox")
                .on_click(cx.listener(move |page, _, _, cx| {
                    if let Some(provider) = provider {
                        page.provider = provider;
                        page.account.update(cx, |input, cx| input.set_text(id.clone(), cx));
                    } else {
                        page.error = Some("This account's provider is not supported by managed Scaffold inference.".into());
                    }
                    cx.notify();
                })))
                .child(Self::action(&theme, format!("crew-revoke-{revoke_id}"), "Revoke shared account…")
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.confirmation = Some(("crew.accounts.revoke", json!({"accountId": revoke_id}), format!("Revoke shared account {revoke_id} for all Crew hosts? Existing local CLI credentials will not be switched or deleted."))); cx.notify();
                    })));
            body = body.child(row);
        }
        if self.accounts.is_empty() { body = body.child(widgets::page_subtitle(&theme, "No shared accounts returned by Agent Auth.")); }
        body = body.child(widgets::page_subtitle(&theme, "Import a local account only with your consent. This copies its credentials into the encrypted shared Agent Auth pool without activating or deleting any local login."));
        for account in self.local_accounts.clone().into_iter().filter(|account| matches!(account.harness, zeron_proto::HarnessId::Codex | zeron_proto::HarnessId::ClaudeCode)) {
            let label = format!("Import local {} account {}…", super::accounts::provider_name(account.harness), account.id);
            body = body.child(Self::action(&theme, format!("crew-import-{:?}-{}", account.harness, account.id), label)
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.confirmation = Some(("crew.accounts.importLocal", json!({"harness": account.harness, "accountId": account.id}), format!("Import local {:?} account {} into Agent Auth for use across your authorized Crew hosts? No local CLI account will be switched.", account.harness, account.id)));
                    cx.notify();
                })));
        }
        body = body.child(widgets::row_title(&theme, "Start a Scaffold sandbox".to_string()))
            .child(Self::field(&theme, "Source ref", &self.source))
            .child(Self::field(&theme, "Region", &self.region))
            .child(Self::field(&theme, "Model ID", &self.model))
            .child(Self::field(&theme, "Opaque account ID (optional)", &self.account));
        let provider = widgets::select("crew-provider", "Model provider", &theme, |page: &mut Self| &mut page.provider_select)
            .options([widgets::SelectOption::new("OpenAI"), widgets::SelectOption::new("Anthropic")], usize::from(self.provider == AgentProvider::Anthropic))
            .on_select(|page, ix, _, cx| { page.provider = if ix == 0 { AgentProvider::OpenAi } else { AgentProvider::Anthropic }; cx.notify(); })
            .render(&self.provider_select, cx);
        let database = widgets::select("crew-database", "Database environment", &theme, |page: &mut Self| &mut page.database_select)
            .options(["Local", "Staging snapshot", "Production snapshot"].into_iter().map(widgets::SelectOption::new), match self.database { ScaffoldDatabaseEnvironment::Local => 0, ScaffoldDatabaseEnvironment::StagingSnapshot => 1, ScaffoldDatabaseEnvironment::ProductionSnapshot => 2 })
            .on_select(|page, ix, _, cx| { page.database = match ix { 1 => ScaffoldDatabaseEnvironment::StagingSnapshot, 2 => ScaffoldDatabaseEnvironment::ProductionSnapshot, _ => ScaffoldDatabaseEnvironment::Local }; cx.notify(); })
            .render(&self.database_select, cx);
        body = body.child(provider).child(database).child(Self::action(&theme, "crew-create", "Create sandbox…").on_click(cx.listener(|page, _, _, cx| page.create(cx))));
        for environment in self.environments.clone() {
            let SessionEnvironmentSource::Scaffold { sandbox_id, lifecycle, .. } = &environment.source else { continue; };
            let mut row = widgets::section_card(&theme).p(px(12.0)).child(widgets::row_title(&theme,
                format!("{} · {:?}", sandbox_id, lifecycle))).child(widgets::page_subtitle(&theme,
                format!("Owner {} · session {} · source {}", environment.owner_principal, environment.scope.session_id.as_deref().unwrap_or("not yet assigned"), environment.source_ref.as_deref().unwrap_or("unknown"))));
            if let Some(link) = crate::links::crew_scope_link(&environment.scope) {
                row = row.child(Self::action(&theme, format!("crew-open-{sandbox_id}"), "Open history (no resume)")
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.state.update(cx, |state, cx| state.open_deep_link(&link, cx));
                    })));
            }
            for operation in ["Inspect", "Attach / resume same sandbox", "Pause sandbox", "Resume sandbox", "Stop sandbox", "Apply model/account selection"] {
                let sandbox = sandbox_id.clone(); let scope = environment.scope.clone();
                row = row.child(Self::action(&theme, format!("crew-{operation}-{sandbox}"), operation).on_click(cx.listener(move |page, _, _, cx| {
                    let control = match operation {
                        "Apply model/account selection" => {
                            let model = page.model.read(cx).text().trim().to_owned();
                            if model.is_empty() { page.error = Some("Enter a model ID in the sandbox form first.".into()); cx.notify(); return; }
                            let account = page.account.read(cx).text().trim().to_owned();
                            let agent_route = if account.is_empty() { AgentRoute::automatic(page.provider, model) }
                                else { AgentRoute::pinned(page.provider, model, account) };
                            ScaffoldEnvironmentControl::UpdateAgentRoute { sandbox_id: sandbox.clone(), scope: scope.clone(), agent_route }
                        }
                        "Inspect" => ScaffoldEnvironmentControl::Inspect { sandbox_id: sandbox.clone(), scope: scope.clone() },
                        "Pause sandbox" => ScaffoldEnvironmentControl::Pause { sandbox_id: sandbox.clone(), scope: scope.clone() },
                        "Resume sandbox" => ScaffoldEnvironmentControl::Resume { sandbox_id: sandbox.clone(), scope: scope.clone() },
                        "Stop sandbox" => ScaffoldEnvironmentControl::Stop { sandbox_id: sandbox.clone(), scope: scope.clone() },
                        _ => ScaffoldEnvironmentControl::Attach { sandbox_id: sandbox.clone(), scope: scope.clone() },
                    };
                    if operation == "Inspect" { page.control(control, cx); }
                    else { page.confirmation = Some(("scaffold.control", json!({"commandId": uuid::Uuid::new_v4().to_string(), "control": control}), format!("{operation}: {sandbox}? Attach resumes a paused sandbox; Stop is terminal. This is separate from stopping an agent turn.\n{}", json!(control)))); cx.notify(); }
                })));
            }
            body = body.child(row);
        }
        body = body.child(Self::field(&theme, "Recovery command ID", &self.command))
            .child(Self::action(&theme, "crew-reconcile", "Inspect and reconcile command (never replay)").on_click(cx.listener(|page, _, _, cx| {
                let id = page.command.read(cx).text().trim().to_owned();
                if !id.is_empty() { page.request("scaffold.reconcile", json!({"commandId": id}), cx); }
            })));
        if let Some(outcome) = &self.outcome {
            body = body.child(widgets::page_subtitle(&theme, format!("Command {}: {}. {}", outcome.command_id, outcome.status, outcome.error.as_deref().unwrap_or("State is the authoritative inspected result, not HTTP acceptance."))));
        }
        if let Some((_, _, description)) = &self.confirmation {
            body = body.child(widgets::section_card(&theme).p(px(12.0)).child(widgets::page_subtitle(&theme, description.clone()))
                .child(Self::action(&theme, "crew-confirm", "Confirm")
                    .tab_index(if self.busy { -1 } else { 0 })
                    .when(self.busy, |el| el.opacity(0.5).aria_description("Unavailable while a request is pending"))
                    .on_click(cx.listener(|page, _, _, cx| {
                    if page.busy { return; }
                    if let Some((method, params, _)) = page.confirmation.take() { page.request(method, params, cx); }
                })))
                .child(Self::action(&theme, "crew-cancel", "Cancel").on_click(cx.listener(|page, _, _, cx| { page.confirmation = None; cx.notify(); }))));
        }
        body.into_any_element()
    }
}
