//! Settings → Advisor: OMP's device-local second-pass reviewer configuration.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*,
    px,
};

use comet_proto::{HarnessId, Model, OmpAdvisorConfig, OmpAdvisorSyncBacklog};
use comet_rpc::methods;

use crate::icons::{self, icon};
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

fn configured_model<'a>(selector: &str, models: &'a [Model]) -> Option<&'a Model> {
    models
        .iter()
        .filter(|model| {
            selector == model.id
                || selector
                    .strip_prefix(&model.id)
                    .is_some_and(|suffix| suffix.starts_with(':'))
        })
        .max_by_key(|model| model.id.len())
}

fn configured_model_label(selector: &str, models: &[Model]) -> String {
    configured_model(selector, models)
        .map(|model| {
            let suffix = selector.strip_prefix(&model.id).unwrap_or_default();
            format!("{}{}", model.label, suffix)
        })
        .unwrap_or_else(|| selector.to_string())
}

pub struct AdvisorPage {
    state: Entity<AppState>,
    config: Loadable<OmpAdvisorConfig>,
    models: Loadable<Vec<Model>>,
    model_menu_open: bool,
    model_menu_dismissed_at: Option<Instant>,
    saving: bool,
    error: Option<SharedString>,
    load_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
}

impl AdvisorPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            config: Loadable::Idle,
            models: Loadable::Idle,
            model_menu_open: false,
            model_menu_dismissed_at: None,
            saving: false,
            error: None,
            load_task: None,
            save_task: None,
        };
        page.load(cx);
        page
    }

    fn cwd(&self, cx: &Context<Self>) -> String {
        self.state
            .read(cx)
            .selected_space_row()
            .map(|space| space.path.clone())
            .unwrap_or_default()
    }

    fn install_config(&mut self, config: OmpAdvisorConfig, cx: &mut Context<Self>) {
        self.config = Loadable::Ready(config);
        self.saving = false;
        self.error = None;
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.config = Loadable::Error("Engine not connected".into());
            self.models = Loadable::Error("Engine not connected".into());
            cx.notify();
            return;
        };
        self.config = Loadable::Loading;
        self.models = Loadable::Loading;
        self.model_menu_open = false;
        self.error = None;
        let cwd = self.cwd(cx);
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let config_result = engine
                .client()
                .call(
                    methods::GET_OMP_ADVISOR_CONFIG,
                    serde_json::json!({ "cwd": cwd }),
                )
                .await
                .and_then(|value| {
                    serde_json::from_value::<OmpAdvisorConfig>(value)
                        .map_err(|error| comet_rpc::RpcError::Failed(error.to_string()))
                });
            let models_result = engine
                .client()
                .call(
                    methods::LIST_MODELS,
                    serde_json::json!({ "harness": HarnessId::Omp }),
                )
                .await
                .and_then(|value| {
                    serde_json::from_value::<Vec<Model>>(value)
                        .map_err(|error| comet_rpc::RpcError::Failed(error.to_string()))
                });
            this.update(cx, |page, cx| {
                match config_result {
                    Ok(config) => page.install_config(config, cx),
                    Err(error) => {
                        page.config = Loadable::Error(error.to_string());
                    }
                }
                page.models = match models_result {
                    Ok(models) => Loadable::Ready(models),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn update_setting(
        &mut self,
        setting: &'static str,
        value: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.saving = true;
        self.error = None;
        let cwd = self.cwd(cx);
        self.save_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SET_OMP_ADVISOR_CONFIG,
                    serde_json::json!({ "cwd": cwd, "setting": setting, "value": value }),
                )
                .await
                .and_then(|value| {
                    serde_json::from_value::<OmpAdvisorConfig>(value)
                        .map_err(|error| comet_rpc::RpcError::Failed(error.to_string()))
                });
            this.update(cx, |page, cx| match result {
                Ok(config) => page.install_config(config, cx),
                Err(error) => {
                    page.saving = false;
                    page.error = Some(error.to_string().into());
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn pick_model(&mut self, model: String, cx: &mut Context<Self>) {
        self.model_menu_open = false;
        self.model_menu_dismissed_at = None;
        self.update_setting("model", serde_json::json!(model), cx);
    }

    fn setting_row(
        theme: &Theme,
        title: &'static str,
        description: &'static str,
        control: AnyElement,
        first: bool,
    ) -> AnyElement {
        div()
            .px(px(18.0))
            .py(px(14.0))
            .when(!first, |row| row.border_t_1().border_color(theme.border))
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
                            .child(SharedString::from(title)),
                    )
                    .child(
                        div()
                            .mt(px(3.0))
                            .text_size(px(11.5))
                            .line_height(px(17.0))
                            .text_color(theme.text_muted.opacity(0.7))
                            .child(SharedString::from(description)),
                    ),
            )
            .child(control)
            .into_any_element()
    }

    fn toggle(theme: &Theme, on: bool, id: &'static str) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .w(px(36.0))
            .h(px(20.0))
            .p(px(2.0))
            .rounded_full()
            .flex()
            .items_center()
            .when(on, |toggle| toggle.justify_end().bg(theme.accent))
            .when(!on, |toggle| {
                toggle.justify_start().bg(crate::theme::ink(0.12))
            })
            .cursor_pointer()
            .child(
                div()
                    .size(px(16.0))
                    .rounded_full()
                    .bg(theme.bg)
                    .border_1()
                    .border_color(theme.border),
            )
    }

    fn choice(
        theme: &Theme,
        label: &'static str,
        selected: bool,
        id: SharedString,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .h(px(26.0))
            .min_w(px(34.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(11.5))
            .text_color(if selected {
                theme.text
            } else {
                theme.text_muted
            })
            .bg(if selected {
                crate::theme::ink(0.08)
            } else {
                gpui::transparent_black()
            })
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::ink(0.06)))
            .child(SharedString::from(label))
    }

    fn render_model_control(
        &mut self,
        config: &OmpAdvisorConfig,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.model_menu_open;
        let saving = self.saving;
        let (label, models): (SharedString, Option<Vec<Model>>) = match &self.models {
            Loadable::Ready(models) => (
                configured_model_label(&config.model, models).into(),
                Some(models.clone()),
            ),
            Loadable::Error(_) => ("Retry models".into(), None),
            Loadable::Idle | Loadable::Loading => ("Loading models…".into(), None),
        };

        let mut trigger = div()
            .id("advisor-model-select")
            .relative()
            .w(px(310.0))
            .h(px(36.0))
            .px(px(12.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(crate::theme::hairline(0.08))
            .bg(crate::theme::ink(0.04))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(px(12.0))
            .text_color(theme.text)
            .cursor_pointer()
            .when(open, |select| select.bg(crate::theme::ink(0.08)))
            .when(!open, |select| {
                select.hover(|style| style.bg(crate::theme::ink(0.06)))
            })
            .when(saving, |select| select.opacity(0.5))
            .on_click(cx.listener(|this, _, _, cx| {
                if this.saving {
                    return;
                }
                match this.models {
                    Loadable::Ready(_) => {
                        let just_dismissed = this
                            .model_menu_dismissed_at
                            .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                        this.model_menu_open = !this.model_menu_open && !just_dismissed;
                        this.model_menu_dismissed_at = None;
                        cx.notify();
                    }
                    Loadable::Error(_) => this.load(cx),
                    Loadable::Idle | Loadable::Loading => {}
                }
            }))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                icon(icons::SORT_VERTICAL)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.45 })),
            );

        if open && let Some(models) = models {
            let selected_id =
                configured_model(&config.model, &models).map(|model| model.id.clone());
            let menu = popover::popover_card(theme)
                .w(px(310.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.model_menu_open = false;
                    this.model_menu_dismissed_at = Some(Instant::now());
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(popover::menu_heading(theme, "Models"))
                .child(
                    div()
                        .id("advisor-model-list")
                        .max_h(px(320.0))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .children(models.into_iter().enumerate().map(|(ix, model)| {
                            let is_selected = selected_id.as_deref() == Some(model.id.as_str());
                            let model_id = model.id.clone();
                            let label: SharedString = model.label.into();
                            let selector: SharedString = model.id.into();
                            popover::menu_row(theme, is_selected, format!("advisor-model-row-{ix}"))
                                .id(("advisor-model-row", ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_model(model_id.clone(), cx);
                                }))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .child(div().truncate().child(label))
                                        .child(
                                            div()
                                                .truncate()
                                                .text_size(px(10.5))
                                                .text_color(theme.text_muted.opacity(0.65))
                                                .child(selector),
                                        ),
                                )
                                .when(is_selected, |row| row.child(popover::menu_check(theme)))
                        })),
                )
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu("advisor-model-menu", menu));
        }

        trigger.into_any_element()
    }

    fn render_ready(
        &mut self,
        config: OmpAdvisorConfig,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enabled = config.enabled;
        let subagents = config.subagents;
        let saving = self.saving;
        let model_control = self.render_model_control(&config, theme, cx);

        let backlog = div()
            .flex_none()
            .flex()
            .flex_row()
            .gap(px(2.0))
            .children(OmpAdvisorSyncBacklog::ALL.into_iter().map(|value| {
                let selected = value == config.sync_backlog;
                let label = match value {
                    OmpAdvisorSyncBacklog::Off => "Off",
                    OmpAdvisorSyncBacklog::One => "1",
                    OmpAdvisorSyncBacklog::Three => "3",
                    OmpAdvisorSyncBacklog::Five => "5",
                };
                Self::choice(
                    theme,
                    label,
                    selected,
                    format!("advisor-backlog-{}", value.value()).into(),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.update_setting("syncBacklog", serde_json::json!(value), cx)
                }))
            }))
            .into_any_element();

        let immune_turns = config.immune_turns;
        let cooldown = div()
            .flex_none()
            .flex()
            .flex_row()
            .gap(px(2.0))
            .children([0_u32, 1, 3, 5].into_iter().map(|value| {
                let label: &'static str = match value {
                    0 => "0",
                    1 => "1",
                    3 => "3",
                    _ => "5",
                };
                Self::choice(
                    theme,
                    label,
                    value == immune_turns,
                    format!("advisor-cooldown-{value}").into(),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.update_setting("immuneTurns", serde_json::json!(value), cx)
                }))
            }))
            .into_any_element();

        widgets::section_card(theme)
            .mt(px(24.0))
            .when(saving, |card| card.opacity(0.75))
            .child(Self::setting_row(
                theme,
                "Advisor",
                "Review each OMP turn with a second model.",
                Self::toggle(theme, enabled, "advisor-enabled")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_setting("enabled", serde_json::json!(!enabled), cx)
                    }))
                    .into_any_element(),
                true,
            ))
            .child(Self::setting_row(
                theme,
                "Model",
                "Choose the second model for OMP reviews.",
                model_control,
                false,
            ))
            .child(Self::setting_row(
                theme,
                "Subagents",
                "Review task and eval subagents too.",
                Self::toggle(theme, subagents, "advisor-subagents")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_setting("subagents", serde_json::json!(!subagents), cx)
                    }))
                    .into_any_element(),
                false,
            ))
            .child(Self::setting_row(
                theme,
                "Catch-up threshold",
                "Pause briefly when the advisor falls this many turns behind.",
                backlog,
                false,
            ))
            .child(Self::setting_row(
                theme,
                "Interrupt cooldown",
                "Primary turns before another concern can interrupt.",
                cooldown,
                false,
            ))
            .into_any_element()
    }
}

impl Render for AdvisorPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let body = match &self.config {
            Loadable::Idle | Loadable::Loading => div()
                .mt(px(28.0))
                .text_size(px(12.5))
                .text_color(theme.text_muted)
                .child(SharedString::from("Loading advisor settings…"))
                .into_any_element(),
            Loadable::Error(message) => {
                let message = message.clone();
                widgets::error_strip(&theme, message)
                    .id("advisor-load-error")
                    .mt(px(24.0))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.load(cx)))
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_size(px(11.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Click to retry")),
                    )
                    .into_any_element()
            }
            Loadable::Ready(config) => self.render_ready(config.clone(), &theme, cx),
        };

        div()
            .id("advisor-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Advisor", None))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Configure OMP's second-pass reviewer on this device.",
                    ))
                    .when_some(self.error.clone(), |column, message| {
                        column.child(
                            widgets::error_strip(&theme, message)
                                .id("advisor-action-error")
                                .mt(px(16.0))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
        }
    }

    #[test]
    fn configured_model_label_preserves_reasoning_suffix() {
        let models = vec![
            model("openai-codex/gpt-5.6", "GPT-5.6"),
            model("openai-codex/gpt-5.6-sol", "GPT-5.6 Sol"),
        ];

        assert_eq!(
            configured_model_label("openai-codex/gpt-5.6-sol:xhigh", &models),
            "GPT-5.6 Sol:xhigh"
        );
    }

    #[test]
    fn configured_model_label_keeps_unknown_selector() {
        assert_eq!(
            configured_model_label("anthropic/custom-model", &[]),
            "anthropic/custom-model"
        );
    }
}
