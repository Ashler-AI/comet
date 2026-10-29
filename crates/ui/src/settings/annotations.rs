//! Retained semantic annotations use host-authorized commands, never raw document writes.
use super::{crew::CrewAccounts, widgets};
use crate::{composer::ComposerInput, state::AppState, theme::Theme};
use gpui::{Context, Entity, Focusable, SharedString, Subscription, Window, div, prelude::*, px};
use zeron_proto::{AnchorTargetKind, AnnotationCommand, SemanticAnchor, SemanticAnnotation};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingAnnotation {
    pub chat_id: String,
    pub owner: String,
    pub scope: zeron_proto::CollaborationScope,
    pub command_id: String,
    pub command: AnnotationCommand,
}

fn pending_for(state: &AppState, cx: &gpui::App) -> Option<(String, AnnotationCommand)> {
    let chat = state.selected_chat_row()?;
    let owner = &state.auth_user()?.id;
    let scope = &chat.crew.as_ref()?.scope;
    super::current(cx)
        .crew_annotation_commands
        .into_iter()
        .find(|pending| {
            pending.chat_id == chat.id && &pending.owner == owner && &pending.scope == scope
        })
        .map(|pending| (pending.command_id, pending.command))
}

pub struct Annotations {
    state: Entity<AppState>,
    chat: Option<String>,
    annotations: Vec<SemanticAnnotation>,
    target: Entity<ComposerInput>,
    exact: Entity<ComposerInput>,
    body: Entity<ComposerInput>,
    editing: Option<String>,
    advanced: bool,
    busy: bool,
    error: Option<String>,
    pending: Option<(String, AnnotationCommand)>,
    _observe: Subscription,
}

impl Annotations {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let chat = state.read(cx).selected_chat.clone();
        let pending = pending_for(state.read(cx), cx);
        let observe = cx.observe(&state, |page: &mut Self, state, cx| {
            let chat = state.read(cx).selected_chat.clone();
            if chat != page.chat {
                page.chat = chat;
                page.annotations.clear();
                page.editing = None;
                page.pending = pending_for(state.read(cx), cx);
                page.error = None;
                page.busy = false;
                page.body.update(cx, |input, cx| input.set_text("", cx));
                page.target.update(cx, |input, cx| input.set_text("", cx));
                page.exact.update(cx, |input, cx| input.set_text("", cx));
                page.advanced = false;
            }
            cx.notify();
        });
        Self {
            state,
            chat,
            annotations: vec![],
            target: cx.new(|cx| ComposerInput::new("Message ID", cx)),
            exact: cx.new(|cx| ComposerInput::new("Exact selected text (optional)", cx)),
            body: cx.new(|cx| ComposerInput::new("Annotation note", cx)),
            editing: None,
            advanced: false,
            busy: false,
            error: None,
            pending,
            _observe: observe,
        }
    }

    pub fn capture_selection(&mut self, cx: &mut Context<Self>) {
        if let Some((id, exact)) = crate::markdown::selection::selected_message_anchor() {
            self.target.update(cx, |input, cx| input.set_text(id, cx));
            self.exact.update(cx, |input, cx| input.set_text(exact, cx));
            self.editing = None;
            self.error = None;
        } else {
            self.error = Some("Select up to 4 KB within one transcript message, or enter its message ID in Advanced options.".into());
        }
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(chat) = self.chat.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Host unavailable; synchronized history remains readable.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call("ListCrewAnnotations", serde_json::json!({"chatId": chat}))
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<SemanticAnnotation>>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |page, cx| {
                if page.chat.as_ref() != Some(&chat) {
                    return;
                }
                page.busy = false;
                match result {
                    Ok(rows) => {
                        page.annotations = rows;
                        page.error = None;
                    }
                    Err(error) => page.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn submit(&mut self, command: AnnotationCommand, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.pending.is_some() {
            self.error = Some("An annotation command has an unknown outcome. Reconcile that command before making another edit.".into());
            cx.notify();
            return;
        }
        self.pending = Some((uuid::Uuid::new_v4().to_string(), command));
        self.send_pending(cx);
    }

    fn send_pending(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some((command_id, command)) = self.pending.clone() else {
            return;
        };
        let Some(chat) = self.chat.clone() else {
            return;
        };
        let state = self.state.read(cx);
        let Some(scope) = state
            .selected_chat_row()
            .and_then(|chat| chat.crew.as_ref())
            .map(|crew| crew.scope.clone())
        else {
            self.error = Some(
                "This local-only session must be imported before adding shared annotations.".into(),
            );
            self.pending = None;
            cx.notify();
            return;
        };
        let Some(owner) = state.auth_user().map(|user| user.id.clone()) else {
            self.error = Some("Sign in to Crew to annotate this session.".into());
            cx.notify();
            return;
        };
        let engine = state.engine().cloned();
        super::update(super::SavePolicy::Immediate, cx, |settings| {
            if !settings
                .crew_annotation_commands
                .iter()
                .any(|pending| pending.command_id == command_id)
            {
                settings.crew_annotation_commands.push(PendingAnnotation {
                    chat_id: chat.clone(),
                    owner,
                    scope: scope.clone(),
                    command_id: command_id.clone(),
                    command: command.clone(),
                });
            }
        });
        let Some(engine) = engine else {
            self.error =
                Some("Host unavailable. Reconnect then reconcile the retained command.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        cx.spawn(async move |this, cx| {
            let result = engine.client().call("ApplyCrewAnnotation", serde_json::json!({"chatId": chat, "scope": scope, "commandId": command_id, "command": command})).await
                .map_err(|error| error.to_string()).and_then(|value| serde_json::from_value::<SemanticAnnotation>(value).map_err(|error| error.to_string()));
            this.update(cx, |page, cx| {
                if page.chat.as_ref() != Some(&chat) { return; }
                page.busy = false;
                match result {
                    Ok(annotation) => {
                        super::update(super::SavePolicy::Immediate, cx, |settings| settings.crew_annotation_commands.retain(|pending| pending.command_id != command_id));
                        page.annotations.retain(|row| row.id != annotation.id); page.annotations.push(annotation);
                        page.pending = None; page.editing = None; page.error = None;
                        page.body.update(cx, |input, cx| input.set_text("", cx));
                    }
                    Err(error) => page.error = Some(format!("Outcome unknown for command {command_id}: {error}. Reconcile uses the same immutable ID.")),
                }
                cx.notify();
            }).ok();
        }).detach();
        cx.notify();
    }
}

impl Render for Annotations {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let mut body = div().flex().flex_col().gap(px(16.0));
        let Some(chat) = self.chat.clone() else {
            return body
                .child("Open a session to view its annotations.")
                .into_any_element();
        };
        let session_title = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.title.clone())
            .unwrap_or_else(|| "Selected session".into());
        body = body.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .id("annotation-session")
                        .min_w_0()
                        .truncate()
                        .tooltip(widgets::text_tooltip(format!("Session {chat}")))
                        .child(widgets::row_title(&theme, session_title)),
                )
                .child(
                    CrewAccounts::action(
                        &theme,
                        "annotations-refresh",
                        if self.busy {
                            "Refreshing…"
                        } else {
                            "Refresh"
                        },
                    )
                    .on_click(cx.listener(|page, _, _, cx| page.refresh(cx))),
                ),
        );
        if let Some(error) = &self.error {
            body = body.child(widgets::error_strip(&theme, error.clone()));
        }
        if self.pending.is_some() {
            body = body.child(
                CrewAccounts::action(
                    &theme,
                    "annotation-reconcile",
                    "Reconcile retained annotation command",
                )
                .on_click(cx.listener(|page, _, _, cx| page.send_pending(cx))),
            );
        }
        let target = self.target.read(cx).text().trim().to_owned();
        let selected_text = self.exact.read(cx).text().to_owned();
        let target_label = if let Some(annotation) = self
            .editing
            .as_ref()
            .and_then(|id| self.annotations.iter().find(|row| &row.id == id))
        {
            format!(
                "Editing note · {:?}: {}",
                annotation.anchor.target_kind, annotation.anchor.target_id
            )
        } else if target.is_empty() {
            "No transcript selection yet".into()
        } else {
            format!("Message {target}")
        };
        body = body
            .when(self.editing.is_none(), |el| {
                el.child(
                    CrewAccounts::action(
                        &theme,
                        "annotations-selection",
                        "Use selected transcript text",
                    )
                    .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(|page, _, _, cx| page.capture_selection(cx))),
                )
            })
            .child(widgets::page_subtitle(&theme, target_label))
            .when(self.editing.is_none() && !selected_text.is_empty(), |el| {
                el.child(
                    div()
                        .max_h(px(88.0))
                        .overflow_hidden()
                        .child(SharedString::from(selected_text)),
                )
            })
            .child(CrewAccounts::field(&theme, "Note", &self.body))
            .child(
                widgets::text_action(
                    &theme,
                    widgets::ActionTone::Filled,
                    if self.editing.is_some() {
                        "Save note"
                    } else {
                        "Create annotation"
                    },
                )
                .id("annotation-save")
                .role(gpui::Role::Button)
                .aria_label(if self.editing.is_some() {
                    "Save note"
                } else {
                    "Create annotation"
                })
                .tab_index(0)
                .self_start()
                .focus_visible(|style| style.border_2().border_color(theme.accent))
                .on_click(cx.listener(|page, _, _, cx| {
                    let body = page.body.read(cx).text().trim().to_owned();
                    if body.is_empty() {
                        page.error = Some("Enter an annotation note.".into());
                        cx.notify();
                        return;
                    }
                    let command = match &page.editing {
                        Some(id) => AnnotationCommand::Edit {
                            id: id.clone(),
                            body,
                        },
                        None => {
                            let target = page.target.read(cx).text().trim().to_owned();
                            if target.is_empty() {
                                page.error = Some(
                                    "Choose a transcript selection or enter a message ID.".into(),
                                );
                                cx.notify();
                                return;
                            }
                            let exact = page.exact.read(cx).text().to_owned();
                            AnnotationCommand::Create {
                                id: uuid::Uuid::new_v4().to_string(),
                                body,
                                anchor: SemanticAnchor {
                                    target_kind: AnchorTargetKind::Message,
                                    target_id: target,
                                    file: None,
                                    byte_range: None,
                                    exact: (!exact.is_empty()).then_some(exact),
                                    prefix_hash: None,
                                    suffix_hash: None,
                                    unknown: Default::default(),
                                },
                            }
                        }
                    };
                    page.submit(command, cx);
                })),
            );
        if self.editing.is_some() {
            body = body.child(
                CrewAccounts::action(&theme, "annotation-cancel-edit", "Cancel editing").on_click(
                    cx.listener(|page, _, _, cx| {
                        page.editing = None;
                        page.body.update(cx, |input, cx| input.set_text("", cx));
                        cx.notify();
                    }),
                ),
            );
        }
        if self.editing.is_none() {
            body = body
                .child(
                    CrewAccounts::action(
                        &theme,
                        "annotation-advanced",
                        if self.advanced {
                            "Hide advanced options"
                        } else {
                            "Advanced options"
                        },
                    )
                    .aria_description("Override the target message ID and exact selected text")
                    .on_click(cx.listener(|page, _, _, cx| {
                        page.advanced = !page.advanced;
                        cx.notify();
                    })),
                )
                .when(self.advanced, |el| {
                    el.child(CrewAccounts::field(&theme, "Message ID", &self.target))
                        .child(CrewAccounts::field(
                            &theme,
                            "Exact selected text (optional)",
                            &self.exact,
                        ))
                });
        }
        for annotation in self.annotations.clone() {
            let edit = annotation.clone();
            let resolve = annotation.clone();
            body = body.child(
                widgets::section_card(&theme)
                    .p(px(12.0))
                    .child(widgets::row_title(
                        &theme,
                        format!(
                            "{} · {:?} · {}",
                            annotation.author_subject,
                            annotation.state,
                            if annotation.resolved_at.is_some() {
                                "Resolved"
                            } else {
                                "Open"
                            }
                        ),
                    ))
                    .child(div().child(SharedString::from(annotation.body)))
                    .child(widgets::page_subtitle(
                        &theme,
                        format!(
                            "{:?}: {}",
                            annotation.anchor.target_kind, annotation.anchor.target_id
                        ),
                    ))
                    .child(
                        CrewAccounts::action(
                            &theme,
                            format!("annotation-edit-{}", annotation.id),
                            "Edit note",
                        )
                        .on_click(cx.listener(
                            move |page, _, window, cx| {
                                page.editing = Some(edit.id.clone());
                                page.body
                                    .update(cx, |input, cx| input.set_text(edit.body.clone(), cx));
                                window.focus(&page.body.focus_handle(cx), cx);
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        CrewAccounts::action(
                            &theme,
                            format!("annotation-resolve-{}", annotation.id),
                            if annotation.resolved_at.is_some() {
                                "Reopen"
                            } else {
                                "Resolve"
                            },
                        )
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.submit(
                                AnnotationCommand::Resolve {
                                    id: resolve.id.clone(),
                                    resolved: resolve.resolved_at.is_none(),
                                },
                                cx,
                            )
                        })),
                    ),
            );
        }
        body.into_any_element()
    }
}
