//! The session card: where this chat runs (device, project, branch), its
//! side-chat actions, and the project's run actions, in one toggleable card.
//! While the column has room, the card shows at its right on its own (until
//! the user hides it), the transcript and composer shifting left as needed
//! to clear it. Once clearing it would narrow the transcript, the card hides,
//! and opening it floats it over the transcript instead.
use super::*;

use crate::project_actions::{ProjectActionsStatus, action_icon};

/// The card's width, its inset from the column's right edge, and the
/// least gap the reading column keeps from it.
pub(super) const SESSION_CARD_WIDTH: f32 = 272.0;
const SESSION_CARD_INSET: f32 = 12.0;
const SESSION_CARD_GAP: f32 = 24.0;
const SESSION_CARDS_GAP: f32 = 8.0;
/// The transcript's own side gutters (its rows pad 48px each side).
const TRANSCRIPT_GUTTER: f32 = 48.0;

/// The room the transcript holds free on its right for the card docked in a
/// main column `main` wide, for a configured reading width `transcript`: none
/// while the centered column already clears the card, else just enough to
/// shift it left. `None` once that would narrow the column: the card no
/// longer fits beside it.
pub(super) fn session_card_reserve(main: f32, transcript: f32) -> Option<f32> {
    let card_left = main - SESSION_CARD_WIDTH - SESSION_CARD_INSET;
    // Centered in `main - reserve`, the column's right edge is
    // (main - reserve) / 2 + transcript / 2; keep it at `card_left - gap`.
    let reserve = (main + transcript - 2.0 * (card_left - SESSION_CARD_GAP)).max(0.0);
    (main - reserve - 2.0 * TRANSCRIPT_GUTTER >= transcript - 0.5).then_some(reserve)
}

#[derive(Default)]
pub(super) struct SessionDiffs {
    diffs: Vec<zeron_proto::CheckoutDiff>,
    target: Option<Option<String>>,
    _watch: Option<Task<()>>,
    base: Option<(String, String)>,
    branch: Option<(String, (u32, u32))>,
    fetched: Option<String>,
    _fetch: Option<Task<()>>,
}

impl SessionDiffs {
    fn checksum(&self, chat: &zeron_proto::Chat) -> Option<String> {
        crate::changes::resolve_diff(&self.diffs, chat).map(|diff| diff.checksum.clone())
    }

    fn totals(&self, chat: &zeron_proto::Chat) -> Option<(u32, u32)> {
        let context = format!("{}|{}", chat.id, chat.cwd.as_deref()?);
        self.branch
            .as_ref()
            .filter(|(key, _)| *key == context)
            .map(|&(_, totals)| totals)
            .filter(|&(adds, dels)| adds + dels > 0)
    }
}

#[derive(Default)]
pub(super) struct SessionPreviews {
    snapshot: zeron_proto::PreviewSnapshot,
    chat: Option<String>,
    _watch: Option<Task<()>>,
    shown: usize,
    scroll: gpui::ScrollHandle,
    bar: popover::MenuScrollbarState,
}

const PORTS_INITIAL: usize = 3;
const PORTS_PAGE: usize = 10;

/// A non-interactive card row: icon slot, label, optional trailing detail.
fn info_row(theme: &Theme, glyph: AnyElement, label: SharedString) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .min_h(px(popover::PICKER_ROW_HEIGHT))
        .px(px(9.0))
        .text_size(crate::typography::ui_rems(13.0))
        .text_color(theme.text)
        .child(popover::picker_icon_slot(glyph))
        .child(div().flex_1().min_w_0().truncate().child(label))
}

fn row_glyph(path: &'static str, theme: &Theme) -> AnyElement {
    icon(path)
        .size(px(15.0))
        .text_color(theme.text_muted)
        .into_any_element()
}

#[derive(Clone, Copy)]
enum SessionEnvironmentLink {
    Web,
    Tilt,
}

fn session_environment_url(
    chat_id: &str,
    crew: Option<&zeron_proto::CrewChatMetadata>,
    link: SessionEnvironmentLink,
) -> Option<String> {
    let crew = crew?;
    crew.validate().ok()?;
    if crew.session_id != chat_id {
        return None;
    }
    let zeron_proto::SessionEnvironmentSource::Scaffold { links, .. } =
        &crew.environment.as_ref()?.source
    else {
        return None;
    };
    let url = match link {
        SessionEnvironmentLink::Web => links.web.as_deref(),
        SessionEnvironmentLink::Tilt => links.tilt.as_deref(),
    }?;
    crate::browser::model::transcript_address(url).ok()
}

impl Shell {
    fn ensure_session_diffs(&mut self, cx: &mut Context<Self>) {
        let (target, engine) = {
            let state = self.state.read(cx);
            let Some(chat) = state.selected_chat_row() else {
                return;
            };
            let device = chat.device_id.clone();
            let target =
                (state.local_device_id.as_deref() != Some(device.as_str())).then_some(device);
            (target, state.engine().cloned())
        };
        if self.session_diffs.target.as_ref() == Some(&target) {
            return;
        }
        let Some(engine) = engine else {
            return;
        };
        self.session_diffs.diffs.clear();
        self.session_diffs.target = Some(target.clone());
        self.session_diffs._watch = Some(cx.spawn(async move |this, cx| {
            loop {
                let mut params = serde_json::Map::new();
                if let Some(target) = &target {
                    params.insert(
                        "targetDeviceId".into(),
                        serde_json::Value::String(target.clone()),
                    );
                }
                if let Ok(mut rx) = engine
                    .client()
                    .subscribe(
                        methods::WATCH_CHECKOUT_DIFFS,
                        serde_json::Value::Object(params),
                    )
                    .await
                {
                    while let Some(value) = rx.recv().await {
                        let alive = this.update(cx, |this, cx| {
                            let chat = this.state.read(cx).selected_chat_row().cloned();
                            let checksum = |diffs: &SessionDiffs| {
                                chat.as_ref().and_then(|chat| diffs.checksum(chat))
                            };
                            let before = checksum(&this.session_diffs);
                            if crate::changes::apply_diff_frame(
                                &mut this.session_diffs.diffs,
                                value,
                            ) && checksum(&this.session_diffs) != before
                            {
                                cx.notify();
                            }
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        }));
    }

    fn ensure_session_previews(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        if self.session_previews.chat.as_deref() == Some(chat_id) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.session_previews = SessionPreviews {
            chat: Some(chat_id.to_owned()),
            ..Default::default()
        };
        let chat_id = chat_id.to_owned();
        self.session_previews._watch = Some(cx.spawn(async move |this, cx| {
            loop {
                if let Ok(mut rx) = engine
                    .client()
                    .subscribe(
                        methods::WATCH_PREVIEWS,
                        serde_json::json!({ "chatId": chat_id }),
                    )
                    .await
                {
                    while let Some(value) = rx.recv().await {
                        let Ok(snapshot) =
                            serde_json::from_value::<zeron_proto::PreviewSnapshot>(value)
                        else {
                            continue;
                        };
                        let alive = this.update(cx, |this, cx| {
                            if this.session_previews.snapshot != snapshot {
                                this.session_previews.snapshot = snapshot;
                                cx.notify();
                            }
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                }
                let alive = this.update(cx, |this, cx| {
                    if !this.session_previews.snapshot.services.is_empty() {
                        this.session_previews.snapshot.services.clear();
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        }));
    }

    fn ensure_branch_totals(&mut self, chat: &zeron_proto::Chat, cx: &mut Context<Self>) {
        let Some(cwd) = chat.cwd.clone() else {
            return;
        };
        let Some(checksum) = self.session_diffs.checksum(chat) else {
            return;
        };
        let context = format!("{}|{cwd}", chat.id);
        let key = format!("{context}|{checksum}");
        if self.session_diffs.fetched.as_deref() == Some(key.as_str()) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let target = self.session_diffs.target.clone().flatten();
        let repo = format!("{}:{cwd}", target.as_deref().unwrap_or("local"));
        let known_base = self
            .session_diffs
            .base
            .as_ref()
            .filter(|(at, _)| *at == repo)
            .map(|(_, base)| base.clone());
        let current = chat.branch.clone();
        self.session_diffs.fetched = Some(key.clone());
        self.session_diffs._fetch = Some(cx.spawn(async move |this, cx| {
            let with_target = |mut params: serde_json::Map<String, serde_json::Value>| {
                if let Some(target) = &target {
                    params.insert(
                        "targetDeviceId".into(),
                        serde_json::Value::String(target.clone()),
                    );
                }
                serde_json::Value::Object(params)
            };
            let base = match known_base {
                Some(base) => Some(base),
                None => {
                    let mut params = serde_json::Map::new();
                    params.insert("repoPath".into(), serde_json::Value::String(cwd.clone()));
                    engine
                        .client()
                        .call(methods::LIST_BRANCHES, with_target(params))
                        .await
                        .ok()
                        .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
                        .and_then(|branches| {
                            crate::changes::default_base_ref(&branches, current.as_deref())
                        })
                }
            };
            let diff = match &base {
                Some(base) => {
                    let mut params = serde_json::Map::new();
                    params.insert("cwd".into(), serde_json::Value::String(cwd.clone()));
                    params.insert("mode".into(), "branch".into());
                    params.insert("baseRef".into(), serde_json::Value::String(base.clone()));
                    engine
                        .client()
                        .call(methods::GET_CHECKOUT_DIFF, with_target(params))
                        .await
                        .ok()
                        .and_then(|value| {
                            serde_json::from_value::<zeron_proto::CheckoutDiff>(value).ok()
                        })
                }
                None => None,
            };
            this.update(cx, |this, cx| {
                let diffs = &mut this.session_diffs;
                if diffs.fetched.as_deref() != Some(key.as_str()) {
                    return;
                }
                if let Some(base) = base {
                    diffs.base = Some((repo, base));
                }
                diffs.branch = diff.map(|diff| (context, (diff.additions, diff.deletions)));
                cx.notify();
            })
            .ok();
        }));
    }

    /// The card's reserve at the main column's LIVE width (see
    /// [`session_card_reserve`]): while the sidebar or a pane animates, the
    /// settled width would move or hide the card ahead of the column.
    fn session_info_reserve(&self, cx: &App) -> Option<f32> {
        let main = (self.viewport_width
            - self.sidebar_now()
            - self.right_visible_width(cx)
            - self.files_visible_width(cx))
        .max(0.0);
        session_card_reserve(main, crate::settings::transcript_width(cx))
    }

    /// Whether the card fits beside the transcript without narrowing it.
    pub(super) fn session_info_fits(&self, cx: &App) -> bool {
        self.session_info_reserve(cx).is_some()
    }

    /// Beside the transcript it shows unless hidden; over it, only while
    /// opened.
    pub(super) fn session_info_visible(&self, cx: &App) -> bool {
        if self.session_info_fits(cx) {
            !self.settings.session_info_hidden
        } else {
            self.session_info_overlay
        }
    }

    /// The toggle's mouse-down: note whether the card showed, so the click
    /// that follows closes it rather than reopening it. Reads the last
    /// painted state, which the overlay's click-outside dismissal (handled
    /// on the same press, in either order) leaves alone.
    pub(super) fn note_session_info_press(&mut self) {
        self.session_info_pressed_open = Some(self.session_info_shown);
    }

    pub(super) fn toggle_session_info(&mut self, cx: &mut Context<Self>) {
        let show = !self
            .session_info_pressed_open
            .take()
            .unwrap_or(self.session_info_shown);
        if self.session_info_fits(cx) {
            self.settings.session_info_hidden = !show;
            self.schedule_save(cx);
        } else {
            self.session_info_overlay = show;
        }
        cx.notify();
    }

    pub(super) fn show_session_info(&mut self, cx: &mut Context<Self>) {
        if !self.session_info_fits(cx) {
            self.session_info_overlay = true;
        } else if self.settings.session_info_hidden {
            self.settings.session_info_hidden = false;
            self.schedule_save(cx);
        }
        cx.notify();
    }

    /// Dismiss the card floating over the transcript; one shown beside it
    /// stays put.
    pub(super) fn close_session_info_overlay(&mut self, cx: &mut Context<Self>) {
        if self.session_info_overlay {
            self.session_info_overlay = false;
            cx.notify();
        }
    }

    fn open_session_environment_link(
        &mut self,
        chat_id: &str,
        link: SessionEnvironmentLink,
        cx: &mut Context<Self>,
    ) {
        let url = self
            .state
            .read(cx)
            .selected_chat_row()
            .filter(|chat| chat.id == chat_id)
            .and_then(|chat| session_environment_url(&chat.id, chat.crew.as_ref(), link));
        if let Some(url) = url {
            self.close_session_info_overlay(cx);
            cx.open_url(&url);
        }
    }

    /// This frame's card: docked beside the transcript (else floating over
    /// it), its reveal, 0 (hidden) → 1 (shown), and the room the transcript
    /// holds free for it. The reveal eases on the shell's resize clock
    /// whenever the card shows or hides, by toggle or by the column crossing
    /// the fit, and the reserve eases with it. A switch between chats lands
    /// it directly, with the destination's panels.
    pub(super) fn session_info_frame(&mut self, has_selection: bool, cx: &App) -> (bool, f32, f32) {
        let reserve = self.session_info_reserve(cx);
        if let Some(reserve) = reserve {
            // An overlay opened on a tighter column ends once the card fits.
            self.session_info_overlay = false;
            self.session_info_last_reserve = reserve;
        }
        let shown = has_selection && self.session_info_visible(cx);
        if shown {
            self.session_info_docked = reserve.is_some();
        }
        let target = if shown { 1.0 } else { 0.0 };
        if std::mem::take(&mut self.session_info_snap) {
            self.session_info_tween = None;
        } else if shown != self.session_info_shown {
            let from = self.eval_tween(self.session_info_tween, 1.0 - target);
            self.session_info_tween = Some(WidthTween::new(from, target));
        }
        self.session_info_shown = shown;
        let reveal = self.eval_tween(self.session_info_tween, target);
        // Hiding as the column stops fitting, the card keeps its docked look
        // and the transcript eases back from the last room it held.
        let docked = self.session_info_docked;
        let reserve = if docked {
            reserve.unwrap_or(self.session_info_last_reserve) * reveal
        } else {
            0.0
        };
        (docked, reveal, reserve)
    }

    /// The card, positioned within the main column; `None` while closed or
    /// with no chat selected.
    pub(super) fn render_session_info(
        &mut self,
        docked: bool,
        reveal: f32,
        max_height: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if reveal <= 0.001 {
            self.session_diffs = SessionDiffs::default();
            self.session_previews = SessionPreviews::default();
            return None;
        }
        let theme = Theme::of(cx).for_popup();
        let now = Utc::now();
        let (chat, device, local, online, project, branch) = {
            let state = self.state.read(cx);
            let chat = state.selected_chat_row()?.clone();
            let device: SharedString = state
                .device_name(&chat.device_id)
                .unwrap_or("Unknown device")
                .to_owned()
                .into();
            let local = state.local_device_id.as_deref() == Some(chat.device_id.as_str());
            let online = state.device_online(&chat.device_id, now);
            let project = chat
                .space_id
                .as_deref()
                .and_then(|id| state.space_row(id))
                .map(|space| {
                    (
                        SharedString::from(space.display_name().to_owned()),
                        SharedString::from(space.path.clone()),
                    )
                });
            let branch = crate::change_requests::conversation_branch(&chat, &state.spaces)
                .map(str::trim)
                .filter(|branch| !branch.is_empty())
                .map(|branch| SharedString::from(branch.to_owned()));
            (chat, device, local, online, project, branch)
        };

        // ── Where it runs.
        let device_row = info_row(
            &theme,
            row_glyph(if local { icons::LAPTOP } else { icons::MONITOR }, &theme),
            device,
        )
        .when(!online, |row| {
            row.child(
                icon(icons::WIFI_OFF)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.warning.opacity(0.8)),
            )
        });
        let project_row = match project {
            Some((name, path)) => info_row(
                &theme,
                self.render_project_icon(&chat.id, 16.0, false, cx),
                name,
            )
            .id("session-info-project")
            .tooltip(crate::settings::widgets::text_tooltip(path))
            .into_any_element(),
            None => info_row(&theme, row_glyph(icons::HOME, &theme), "No project".into())
                .into_any_element(),
        };
        self.ensure_session_diffs(cx);
        self.ensure_branch_totals(&chat, cx);
        let totals = self.session_diffs.totals(&chat);
        let branch_row = branch.map(|branch| {
            info_row(&theme, row_glyph(icons::GIT_BRANCH, &theme), branch.clone())
                .id("session-info-branch")
                .tooltip(crate::settings::widgets::text_tooltip(branch))
                .children(totals.map(|(adds, dels)| {
                    let stat = |text: String, color: gpui::Hsla| {
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(11.0))
                            .text_color(color)
                            .child(SharedString::from(text))
                    };
                    div()
                        .id("session-info-diff")
                        .debug_selector(|| "session-info-diff".into())
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .px(px(5.0))
                        .mr(px(-5.0))
                        .py(px(2.0))
                        .rounded(px(5.0))
                        .cursor_pointer()
                        .hover(|style| style.bg(crate::theme::ink(0.08)))
                        .tooltip(crate::settings::widgets::text_tooltip("Branch changes"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_session_info_overlay(cx);
                            this.add_branch_diff_surface(window, cx);
                        }))
                        .child(stat(format!("+{adds}"), crate::changes::add_color(&theme)))
                        .child(stat(format!("−{dels}"), crate::changes::del_color(&theme)))
                }))
        });

        let environment_links = [
            (
                "crew-session-web",
                "Web",
                "Open Web in browser",
                SessionEnvironmentLink::Web,
            ),
            (
                "crew-session-tilt",
                "Tilt",
                "Open Tilt in browser",
                SessionEnvironmentLink::Tilt,
            ),
        ]
        .map(|(id, label, aria_label, link)| {
            session_environment_url(&chat.id, chat.crew.as_ref(), link)?;
            let click_chat = chat.id.clone();
            let key_chat = chat.id.clone();
            Some(
                popover::picker_row(&theme, false, false, id)
                    .id(id)
                    .role(gpui::Role::Link)
                    .aria_label(aria_label)
                    .tab_index(0)
                    .focus_visible(|style| style.border_2().border_color(theme.accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_session_environment_link(&click_chat, link, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                        if !event.is_held
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            cx.stop_propagation();
                            this.open_session_environment_link(&key_chat, link, cx);
                        }
                    }))
                    .child(popover::picker_icon_slot(row_glyph(icons::GLOBE, &theme)))
                    .child(div().flex_1().child(label)),
            )
        });

        // ── This conversation.
        let busy = self.side_chat_creating;
        let side_chat = popover::picker_row(&theme, false, false, "session-info-side-chat")
            .id("session-info-side-chat")
            .debug_selector(|| "session-info-side-chat".into())
            .when(busy, |row| row.opacity(0.5))
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_session_info_overlay(cx);
                this.create_child_chat(None, cx)
            }))
            .child(popover::picker_icon_slot(row_glyph(icons::PLUS, &theme)))
            .child(div().flex_1().child("New side chat"));
        let fork = popover::picker_row(&theme, false, false, "session-info-fork")
            .id("session-info-fork")
            .debug_selector(|| "session-info-fork".into())
            .when(busy, |row| row.opacity(0.5))
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_session_info_overlay(cx);
                this.create_side_chat(cx)
            }))
            .child(popover::picker_icon_slot(row_glyph(icons::FORK, &theme)))
            .child(div().flex_1().child("Fork chat"));

        // ── The project's run actions.
        let actions = self.render_session_info_actions(&theme, cx);
        let lanes =
            self.render_session_lanes(&chat, !docked, self.viewport_height / 2.0, &theme, cx);

        let body = div()
            .id("session-info-body")
            .max_h(px(max_height.max(120.0)))
            .overflow_y_scroll()
            // The card's inset lives inside the scroller, so dividers can
            // bleed through it edge to edge without being clipped.
            .p(px(popover::CARD_INSET))
            .flex()
            .flex_col()
            .gap(px(popover::MENU_GAP))
            .child(device_row)
            .child(project_row)
            .children(branch_row)
            .when_some(chat.crew.as_ref(), |body, crew| {
                let label = match crew
                    .environment
                    .as_ref()
                    .map(|environment| &environment.source)
                {
                    Some(zeron_proto::SessionEnvironmentSource::Scaffold { lifecycle, .. }) => {
                        format!(
                            "{} · {}",
                            crew.environment
                                .as_ref()
                                .and_then(|environment| environment.name.as_deref())
                                .unwrap_or("Scaffold sandbox"),
                            crate::settings::crew::lifecycle_label(*lifecycle)
                        )
                    }
                    _ => {
                        if local {
                            "Local host".into()
                        } else {
                            "Remote host".into()
                        }
                    }
                };
                body.child(
                    info_row(&theme, row_glyph(icons::MONITOR, &theme), label.into())
                        .id("crew-session-environment")
                        .tooltip(crate::settings::widgets::text_tooltip(format!(
                            "Owner {}\nProject {}\nDeployment {}\nSession {}",
                            crew.owner_subject,
                            crew.scope.project_id,
                            crew.scope.deployment_id.as_deref().unwrap_or("none"),
                            crew.session_id
                        ))),
                )
            })
            .children(environment_links.into_iter().flatten())
            .when(!online, |body| {
                body.child(crate::settings::widgets::page_subtitle(
                    &theme,
                    "Host offline. Synced history remains available; continuing requires its host.",
                ))
            })
            .when(self.state.read(cx).managed_inference, |body| {
                body.child(
                    crate::settings::widgets::ghost_action(&theme)
                        .id("crew-session-sandboxes")
                        .role(gpui::Role::Button)
                        .tab_index(0)
                        .aria_label("Manage Scaffold sandboxes")
                        .focus_visible(|style| style.border_2().border_color(theme.accent))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.close_session_info_overlay(cx);
                            this.open_settings(SettingsSection::Sandboxes, cx);
                        }))
                        .child("Manage sandboxes"),
                )
            })
            .when(chat.crew.is_some(), |body| {
                body.child(
                    crate::settings::widgets::ghost_action(&theme)
                        .id("crew-session-controls")
                        .role(gpui::Role::Button)
                        .aria_label("Shared annotations")
                        .tab_index(0)
                        .focus_visible(|style| style.border_2().border_color(theme.accent))
                        .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.accounts_page.is_none() {
                                let state = this.state.clone();
                                this.accounts_page =
                                    Some(cx.new(|cx| AccountsPage::new(state, cx)));
                            }
                            if crate::markdown::selection::selected_message_anchor().is_some()
                                && let Some(page) = &this.accounts_page
                            {
                                page.update(cx, |page, cx| page.capture_annotation_selection(cx));
                            }
                            this.open_settings(SettingsSection::Annotations, cx);
                        }))
                        .child("Shared annotations"),
                )
            })
            .child(popover::picker_divider())
            .child(side_chat)
            .child(fork)
            .children(actions);

        let ports =
            self.render_session_ports(&chat.id, !docked, self.viewport_height / 3.0, &theme, cx);

        let card = div()
            .w(px(SESSION_CARD_WIDTH))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .rounded(px(popover::CARD_RADIUS))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(popover::surface_bg(&theme))
            .when(!docked && !theme.is_frost(), |card| card.shadow_lg())
            .child(body);
        let frosted =
            |card| crate::frost::frosted(popover::CARD_RADIUS, crate::frost::MENU_BLUR, card);

        // Docked, the card slides in from the right beside the column;
        // floating, it drops in from just above. Either way it is placed from
        // the column's right edge, so it rides that edge while panes animate.
        let ease = crate::motion::RESIZE.progress(reveal.clamp(0.0, 1.0));
        let (dx, dy) = if docked {
            (12.0 * (1.0 - ease), 0.0)
        } else {
            (0.0, -6.0 * (1.0 - ease))
        };
        Some(
            div()
                .id("session-info")
                .debug_selector(|| "session-info".into())
                .absolute()
                .top(px(Theme::TITLEBAR_HEIGHT + 12.0 + dy))
                .right(px(SESSION_CARD_INSET - dx))
                .max_h(px(max_height.max(240.0)))
                .flex()
                .flex_col()
                .gap(px(SESSION_CARDS_GAP))
                .opacity(ease)
                .occlude()
                // Over the transcript it is a popover: a press elsewhere, or
                // choosing one of its rows, dismisses it.
                .when(!docked, |card| {
                    card.on_mouse_down_out(
                        cx.listener(|this, _, _, cx| this.close_session_info_overlay(cx)),
                    )
                })
                .child(frosted(card.into_any_element()))
                .children(ports.map(frosted))
                .child(frosted(lanes))
                .into_any_element(),
        )
    }

    fn render_session_ports(
        &mut self,
        chat_id: &str,
        floating: bool,
        max_height: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.ensure_session_previews(chat_id, cx);
        let snapshot = &self.session_previews.snapshot;
        if snapshot.services.is_empty() {
            return None;
        }
        let note = |text: SharedString| {
            div()
                .px(px(9.0))
                .py(px(5.0))
                .text_size(crate::typography::ui_rems(12.0))
                .line_height(px(18.0))
                .text_color(theme.text_muted)
                .child(text)
                .into_any_element()
        };
        let mut rows: Vec<AnyElement> =
            vec![popover::menu_heading(theme, "Ports").into_any_element()];
        let available = snapshot.error.is_none();
        let shown = self.session_previews.shown.max(PORTS_INITIAL);
        let hidden = snapshot.services.len().saturating_sub(shown);
        for service in snapshot.services.iter().take(shown) {
            let url = service.url(snapshot.proxy_port);
            let row_id = SharedString::from(format!("session-info-preview-{}", service.id));
            let port = SharedString::from(format!(":{}", service.port));
            let detail = SharedString::from(if snapshot.remote {
                format!("{} · {url}", service.device_name)
            } else {
                url.clone()
            });
            rows.push(
                popover::picker_row(theme, false, false, row_id.clone())
                    .id(row_id)
                    .tooltip(crate::settings::widgets::text_tooltip(detail))
                    .when(!available, |row| row.opacity(0.5).cursor_default())
                    .when(available, |row| {
                        row.on_click(cx.listener(move |this, _, window, cx| {
                            this.close_session_info_overlay(cx);
                            this.browser_context.register_preview(&url);
                            this.set_surfaces_open(true, cx);
                            this.add_browser_surface(Some(url.clone()), window, cx);
                        }))
                    })
                    .child(popover::picker_icon_slot(row_glyph(icons::GLOBE, theme)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(service.name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(port),
                    )
                    .into_any_element(),
            );
        }
        if hidden > 0 {
            rows.push(
                popover::picker_row(theme, false, false, "session-ports-more")
                    .id("session-ports-more")
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        let previews = &mut this.session_previews;
                        previews.shown = previews.shown.max(PORTS_INITIAL) + PORTS_PAGE;
                        cx.notify();
                    }))
                    .child(popover::picker_icon_slot(div().into_any_element()))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(format!("Show {} more", hidden.min(PORTS_PAGE))),
                    )
                    .into_any_element(),
            );
        }
        // Listed but unreachable: why.
        if let Some(error) = &snapshot.error {
            rows.push(note(error.clone().into()));
        }
        let body = div()
            .id("session-ports-body")
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.session_previews.scroll)
            .p(px(popover::CARD_INSET))
            .flex()
            .flex_col()
            .gap(px(popover::MENU_GAP))
            .children(rows);
        // The rail shows only once the ports outgrow the card and scroll.
        let scrollbar = popover::rail_for(
            self,
            popover::RailAccess {
                bar: |this: &mut Self| &mut this.session_previews.bar,
                scroll: |this: &Self| this.session_previews.scroll.clone(),
            },
            "session-ports-scrollbar",
            theme,
            cx,
        );
        Some(
            div()
                .id("session-ports")
                .debug_selector(|| "session-ports".into())
                .relative()
                .w(px(SESSION_CARD_WIDTH))
                .max_h(px(max_height))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .rounded(px(popover::CARD_RADIUS))
                .overflow_hidden()
                .border_1()
                .border_color(theme.border)
                .bg(popover::surface_bg(theme))
                .when(floating && !theme.is_frost(), |card| card.shadow_lg())
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    if this.session_previews.bar.set_list_hovered(*hovered) {
                        cx.notify();
                    }
                }))
                .child(body)
                .children(scrollbar)
                .into_any_element(),
        )
    }

    /// The Actions section: a row per project action (click runs it; the
    /// gear edits it), imports from zeron.json, and Add action.
    fn render_session_info_actions(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.ensure_project_actions(cx);
        let (Some(status), Some(snapshot), Some(key)) = (
            self.project_actions.active_status().cloned(),
            self.project_actions.visible_snapshot(),
            self.project_actions.active.clone(),
        ) else {
            return Vec::new();
        };
        let mut rows: Vec<AnyElement> = vec![
            popover::picker_divider().into_any_element(),
            popover::menu_heading(theme, "Actions").into_any_element(),
        ];
        let note = |text: SharedString, color| {
            div()
                .px(px(9.0))
                .py(px(5.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        match &status {
            ProjectActionsStatus::Idle | ProjectActionsStatus::Loading => {
                rows.push(note("Loading actions…".into(), theme.text_muted));
                return rows;
            }
            ProjectActionsStatus::Unavailable { message, .. } => {
                rows.push(note(message.clone().into(), theme.text_muted));
                rows.push(
                    popover::picker_row(theme, false, false, "session-info-actions-retry")
                        .id("session-info-actions-retry")
                        .on_click(cx.listener(|this, _, _, cx| this.retry_project_actions(cx)))
                        .child(popover::picker_icon_slot(row_glyph(icons::REFRESH, theme)))
                        .child("Retry")
                        .into_any_element(),
                );
            }
            _ => {}
        }
        let can_run = status.can_run();
        for action in snapshot.actions.clone() {
            let row_id = SharedString::from(format!("session-info-action-{}", action.id));
            let group = SharedString::from(format!("session-info-action-group-{}", action.id));
            let key = key.clone();
            let run = action.clone();
            let edit = action.clone();
            let label: SharedString = if action.run_on_worktree_create {
                format!("{} (setup)", action.name).into()
            } else {
                action.name.clone().into()
            };
            rows.push(
                popover::picker_row(theme, false, false, row_id.clone())
                    .id(row_id)
                    .group(group.clone())
                    .when(!can_run, |row| row.opacity(0.5).cursor_default())
                    .when(can_run, |row| {
                        row.on_click(cx.listener(move |this, _, _, cx| {
                            this.close_session_info_overlay(cx);
                            this.run_project_action(&key, run.clone(), cx)
                        }))
                    })
                    .child(popover::picker_icon_slot(row_glyph(
                        action_icon(action.icon),
                        theme,
                    )))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "session-info-edit-action-{}",
                                edit.id
                            )))
                            .size(px(22.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .invisible()
                            .group_hover(group, |style| style.visible())
                            .hover(|style| style.bg(crate::theme::ink(0.08)))
                            .tooltip(crate::settings::widgets::text_tooltip("Edit action"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_session_info_overlay(cx);
                                this.open_project_action_editor(Some(edit.clone()), None, cx)
                            }))
                            .child(
                                icon(icons::SETTINGS_MINIMALISTIC)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .into_any_element(),
            );
        }
        for draft in snapshot.importable_actions.clone() {
            let row_id = SharedString::from(format!("session-info-import-{}", draft.name));
            let import = draft.clone();
            rows.push(
                popover::picker_row(theme, false, false, row_id.clone())
                    .id(row_id)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_session_info_overlay(cx);
                        this.open_project_action_editor(None, Some(import.clone()), cx)
                    }))
                    .child(popover::picker_icon_slot(row_glyph(
                        action_icon(draft.icon),
                        theme,
                    )))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(draft.name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .child("Import"),
                    )
                    .into_any_element(),
            );
        }
        if let Some(issue) = snapshot.project_file_issue.clone() {
            rows.push(note(issue.into(), theme.text_muted));
        }
        rows.push(
            popover::picker_row(theme, false, false, "session-info-add-action")
                .id("session-info-add-action")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close_session_info_overlay(cx);
                    this.open_project_action_editor(None, None, cx)
                }))
                .child(popover::picker_icon_slot(row_glyph(icons::PLUS, theme)))
                .child("Add action")
                .into_any_element(),
        );
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_links_require_matching_identity_and_safe_independent_urls() {
        use SessionEnvironmentLink::{Tilt, Web};
        let scope = serde_json::json!({
            "projectId": "project", "deploymentId": "deployment", "sessionId": "session"
        });
        let mut crew: zeron_proto::CrewChatMetadata = serde_json::from_value(serde_json::json!({
            "ownerSubject": "owner", "scope": scope, "sessionId": "session",
            "environment": {
                "ownerPrincipal": "owner", "scope": scope,
                "source": { "kind": "scaffold", "sandbox_id": "sandbox", "lifecycle": "ready",
                    "links": { "web": "https://example.com/app", "tilt": "http://localhost:10350/" } }
            }
        })).unwrap();
        assert_eq!(
            session_environment_url("session", Some(&crew), Web).as_deref(),
            Some("https://example.com/app")
        );
        assert_eq!(
            session_environment_url("session", Some(&crew), Tilt).as_deref(),
            Some("http://localhost:10350/")
        );
        assert!(session_environment_url("other-session", Some(&crew), Web).is_none());
        assert!(session_environment_url("session", None, Web).is_none());

        for url in [
            None,
            Some("javascript:alert(1)"),
            Some("https://user:pass@example.com/"),
            Some("https:///missing-authority"),
            Some("https://example.com/\npath"),
            Some("example.com"),
        ] {
            if let zeron_proto::SessionEnvironmentSource::Scaffold { links, .. } =
                &mut crew.environment.as_mut().unwrap().source
            {
                links.web = url.map(str::to_owned);
            }
            assert!(
                session_environment_url("session", Some(&crew), Web).is_none(),
                "{url:?}"
            );
            assert!(session_environment_url("session", Some(&crew), Tilt).is_some());
        }
        crew.environment.as_mut().unwrap().owner_principal = "other-owner".into();
        assert!(session_environment_url("session", Some(&crew), Tilt).is_none());
        crew.environment.as_mut().unwrap().owner_principal = "owner".into();
        crew.environment.as_mut().unwrap().scope.deployment_id = Some("other-deployment".into());
        assert!(session_environment_url("session", Some(&crew), Tilt).is_none());
        crew.environment.as_mut().unwrap().scope = crew.scope.clone();
        crew.environment.as_mut().unwrap().source = zeron_proto::SessionEnvironmentSource::Local;
        assert!(session_environment_url("session", Some(&crew), Tilt).is_none());
        crew.environment = None;
        assert!(session_environment_url("session", Some(&crew), Tilt).is_none());
    }

    #[test]
    fn a_wide_column_keeps_the_transcript_centered_beside_the_card() {
        assert_eq!(session_card_reserve(1600.0, 736.0), Some(0.0));
        // The centered column's right edge clears the card by the gap.
        let main = 1600.0;
        let card_left = main - SESSION_CARD_WIDTH - SESSION_CARD_INSET;
        assert!(main / 2.0 + 736.0 / 2.0 <= card_left - SESSION_CARD_GAP);
    }

    #[test]
    fn a_tighter_column_shifts_the_transcript_just_enough() {
        let main = 1200.0;
        let reserve = session_card_reserve(main, 736.0).expect("it still fits");
        assert!(reserve > 0.0);
        let card_left = main - SESSION_CARD_WIDTH - SESSION_CARD_INSET;
        let right = (main - reserve) / 2.0 + 736.0 / 2.0;
        assert!((right - (card_left - SESSION_CARD_GAP)).abs() < 0.01);
        // At full width: the column keeps its configured width.
        assert!(main - reserve - 2.0 * TRANSCRIPT_GUTTER >= 736.0);
    }

    #[test]
    fn the_card_stops_fitting_where_the_transcript_would_narrow() {
        // Gutter, column, gap, card, inset.
        let least =
            TRANSCRIPT_GUTTER + 736.0 + SESSION_CARD_GAP + SESSION_CARD_WIDTH + SESSION_CARD_INSET;
        assert!(session_card_reserve(least, 736.0).is_some());
        assert!(session_card_reserve(least - 1.0, 736.0).is_none());
        assert!(session_card_reserve(700.0, 736.0).is_none());
        // A narrower reading column leaves room sooner.
        assert!(session_card_reserve(least - 1.0, 560.0).is_some());
    }
}
