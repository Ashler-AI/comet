//! Folder and session navigation: device-bound folders, the unified Sessions
//! list, and the add-folder palette (⌘K-style device tabs + filtered browser).
//!
//! The storage model calls a synced `(device, folder)` pair a space. The UI
//! presents that implementation detail as a folder and identifies remote hosts
//! only when location matters.

use super::*;
use crate::motion::TAB_SLIDE;
use crate::pickers::{breadcrumbs, browser_rows, parent_path};
use crate::popover::Loadable;
use crate::terminal::panel::{drop_index, reorder_tabs, slide_offset};
use comet_proto::{ChatIndicator, Device, FolderListing, Space};
use gpui::FocusHandle;
use std::path::{Path, PathBuf};

/// Space-row slot height for drag drop-index math: py(6)×2 + 17px line ≈ 29,
/// plus the 2px column gap.
const SPACE_ROW_SLOT: f32 = 31.0;

fn detached_worktree_label(cwd: &str) -> Option<String> {
    let parts: Vec<String> = Path::new(cwd)
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let worktrees = parts.iter().position(|part| part == "worktrees")?;
    let suffix = &parts[worktrees + 1..];
    let label = match suffix {
        [id, _repo, ..] if id.chars().all(|character| character.is_ascii_digit()) => id,
        [.., name] => name,
        [] => return None,
    };
    Some(format!("{label} (detached)"))
}

fn folder_device_name(name: Option<&str>) -> &str {
    name.map(str::trim)
        .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case("unknown-device"))
        .unwrap_or("Remote device")
}

/// One logical source in the sidebar. `space` is presentation-only: every
/// persisted member row remains in `member_ids`, and none of their chats move.
#[derive(Clone, Debug)]
struct SidebarSource {
    space: Space,
    member_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum SourceIdentity {
    Repository(PathBuf),
    Checkout(String),
    Path(PathBuf),
    Space(String),
}

fn normalized_path(path: &Path) -> Option<PathBuf> {
    path.is_absolute()
        .then(|| path.components().collect::<PathBuf>())
}

/// Resolve the Git common directory shared by a repository's main checkout and
/// all of its linked worktrees. This only runs for spaces owned by this device;
/// remote paths are intentionally never inspected or inferred.
fn git_common_directory(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let marker = std::fs::read_to_string(&dot_git).ok()?;
        let path = marker.trim().strip_prefix("gitdir:")?.trim();
        let path = PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    };
    let git_dir = std::fs::canonicalize(&git_dir)
        .ok()
        .or_else(|| normalized_path(&git_dir))?;
    let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|path| PathBuf::from(path.trim()))
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                git_dir.join(path)
            }
        })
        .unwrap_or(git_dir);
    std::fs::canonicalize(&common_dir)
        .ok()
        .or_else(|| normalized_path(&common_dir))
}

/// Recover the main checkout for deleted worktrees created by known agent
/// runtimes. Their `.git` file disappears with the worktree, but the runtime
/// path still carries the project name.
fn materialized_project_common_directory(path: &Path) -> Option<PathBuf> {
    let components: Vec<_> = path.components().collect();
    let (marker_index, project_offset) =
        components
            .iter()
            .enumerate()
            .find_map(|(index, component)| {
                let marker = component.as_os_str().to_str()?;
                (components.get(index + 1)?.as_os_str() == "worktrees").then_some(match marker {
                    ".codex" => (index, Some(3)),
                    ".t3" | "t3" => (index, Some(2)),
                    ".claude" => (index, None),
                    _ => return None,
                })
            })?;
    let prefix: PathBuf = components[..marker_index].iter().collect();
    let Some(project_offset) = project_offset else {
        return git_common_directory(&prefix);
    };
    let project = components.get(marker_index + project_offset)?.as_os_str();
    prefix
        .ancestors()
        .find_map(|ancestor| git_common_directory(&ancestor.join(project)))
}

fn source_representative_rank(space: &Space) -> u8 {
    let dot_git = Path::new(&space.path).join(".git");
    if dot_git.is_dir() {
        2
    } else if dot_git.is_file() {
        1
    } else {
        0
    }
}

/// Collapse local materializations into logical sidebar sources without
/// mutating the shared `Space` projection. The first occurrence fixes the
/// source's visual position; the newest row (then lexicographically smallest
/// id) is its deterministic representative.
fn collapse_sidebar_sources(
    spaces: Vec<Space>,
    local_device_id: Option<&str>,
    mut repository_identity: impl FnMut(&Space) -> Option<PathBuf>,
) -> Vec<SidebarSource> {
    let mut source_indexes: std::collections::HashMap<SourceIdentity, usize> =
        std::collections::HashMap::new();
    let mut sources: Vec<SidebarSource> = Vec::new();

    for space in spaces {
        let identity = if local_device_id == Some(space.device_id.as_str()) {
            repository_identity(&space)
                .map(SourceIdentity::Repository)
                .or_else(|| space.checkout_id.clone().map(SourceIdentity::Checkout))
                .or_else(|| normalized_path(Path::new(&space.path)).map(SourceIdentity::Path))
                .unwrap_or_else(|| SourceIdentity::Space(space.id.clone()))
        } else {
            // Equal paths or checkout ids on different/remote devices are not
            // proof of a shared local repository.
            SourceIdentity::Space(space.id.clone())
        };

        if let Some(index) = source_indexes.get(&identity).copied() {
            let source = &mut sources[index];
            source.member_ids.push(space.id.clone());
            let current_rank = source_representative_rank(&source.space);
            let candidate_rank = source_representative_rank(&space);
            if candidate_rank > current_rank
                || (candidate_rank == current_rank
                    && (space.created_at > source.space.created_at
                        || (space.created_at == source.space.created_at
                            && space.id < source.space.id)))
            {
                source.space = space;
            }
        } else {
            let index = sources.len();
            source_indexes.insert(identity, index);
            sources.push(SidebarSource {
                member_ids: vec![space.id.clone()],
                space,
            });
        }
    }
    for source in &mut sources {
        source.member_ids.sort();
    }
    sources
}

fn source_picker_spaces(spaces: Vec<Space>, local_device_id: Option<&str>) -> Vec<SidebarSource> {
    collapse_sidebar_sources(spaces, local_device_id, |space| {
        let path = Path::new(&space.path);
        git_common_directory(path).or_else(|| materialized_project_common_directory(path))
    })
}
fn sidebar_session_source(
    local_device_id: Option<&str>,
    chat_device_id: &str,
    scaffold_session: bool,
    agent_source: Option<comet_proto::AgentSessionSource>,
) -> comet_proto::AgentSessionSource {
    agent_source.unwrap_or_else(|| {
        if scaffold_session || local_device_id != Some(chat_device_id) {
            comet_proto::AgentSessionSource::Scaffold
        } else {
            comet_proto::AgentSessionSource::Local
        }
    })
}
fn spaces_with_visible_sessions(
    spaces: Vec<Space>,
    visible_space_ids: &std::collections::HashSet<String>,
    retained_space_ids: &[String],
    selected_space_id: Option<&str>,
) -> Vec<Space> {
    spaces
        .into_iter()
        .filter(|space| {
            visible_space_ids.contains(&space.id)
                || retained_space_ids.iter().any(|id| id == &space.id)
                || selected_space_id == Some(space.id.as_str())
        })
        .collect()
}

fn sidebar_branch_label(chat: &comet_proto::Chat) -> Option<String> {
    let branch = chat
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())?;
    if branch == "HEAD" {
        return Some(
            chat.cwd
                .as_deref()
                .and_then(detached_worktree_label)
                .unwrap_or_else(|| "Detached".into()),
        );
    }
    Some(branch.to_string())
}

/// Drag-reorder state for the spaces list; `epoch` keys the 150ms slide
/// animation restarts (the session-tab idiom, vertical).
pub(super) struct SpaceDragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// The dragged-row payload (gpui drag-and-drop).
struct SpaceDragPayload {
    from: usize,
    name: SharedString,
}

/// The floating row rendered at the cursor while dragging.
struct SpaceGhost {
    name: SharedString,
}

impl Render for SpaceGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .w(px(200.0))
            .h(px(29.0))
            .px(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(13.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text)
            .opacity(0.85)
            .child(
                icon(icons::FOLDER)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(div().truncate().child(self.name.clone()))
    }
}

/// The add-space palette (a command-K surface, summoned by ⌘K): search bar
/// across the top, folder browser on the left, a Devices rail on the right,
/// kbd-hint footer. One surface — picking a device in the rail rebrowses in
/// place, no step wizard.
pub(super) struct AddSpaceFlow {
    /// The device currently browsed (the highlighted rail row).
    device: Option<Device>,
    /// Filter input; Enter descends into the highlighted folder.
    search: Entity<ComposerInput>,
    browser: Loadable<FolderListing>,
    /// Requested browser path (`None` = the device's default, i.e. home).
    browser_path: Option<String>,
    /// The device's home (the path a `None` browse resolved to) — breadcrumbs
    /// fold everything up to here into the device-name crumb.
    home: Option<String>,
    /// Best-effort git seed for the CURRENT browser path (known when we
    /// descended through an entry whose `is_repo` we saw; the owning device's
    /// SpacesSync re-verifies either way).
    browser_repo: bool,
    /// Keyboard highlight within the FILTERED folder rows.
    active: usize,
    submit_busy: bool,
    error: Option<SharedString>,
    /// Tracked on the card (`track_focus`) — puts the card on the keyboard
    /// dispatch path so ↑↓/⌫/esc reach `add_space_key` while the search input
    /// holds focus (the structure every working picker uses).
    focus: FocusHandle,
    /// Folder-list scroll — keyboard navigation keeps the highlighted row in
    /// view (`scroll_to_item`).
    list_scroll: gpui::ScrollHandle,
    focus_pending: bool,
    load_task: Option<Task<()>>,
    submit_task: Option<Task<()>>,
    _search_events: Subscription,
}

/// The space-row Rename dialog (same shape as [`RenameChatDialog`]).
pub(super) struct RenameSpaceDialog {
    pub space_id: String,
    pub input: Entity<ComposerInput>,
    pub focus_pending: bool,
    pub _events: Subscription,
}

/// Dot color for a chat's display status (tab dots + Sessions rows).
pub(super) fn status_dot_color(status: ChatIndicator, theme: &Theme) -> gpui::Hsla {
    match status {
        // Pink, not amber — the harsh yellow read as a warning; running is
        // routine (user request).
        ChatIndicator::Working => {
            theme.busy.opacity(0.85) // pink-400
        }
        // Blue: "asking you a question" must read differently from "busy
        // working" at a glance.
        ChatIndicator::AwaitingInput => theme.accent.opacity(0.9),
        ChatIndicator::Errored => theme.danger,
        // Green: finished-but-unseen reads as "ready for you".
        ChatIndicator::Completed => {
            theme.success.opacity(0.9) // emerald-400
        }
        ChatIndicator::Idle => crate::theme::ink(0.14),
    }
}

impl Shell {
    // ---- space switching ----

    /// Land in one persisted space. Logical source rows use
    /// [`Self::activate_source`] with every member id.
    pub(super) fn activate_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.activate_source(space_id.clone(), vec![space_id], cx);
    }

    /// Land in a logical source: remembered tab if alive, else its most recent
    /// chat across every member space, else the new-session canvas.
    fn activate_source(
        &mut self,
        space_id: String,
        member_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.route = Route::Chat;
        self.state.update(cx, |state, cx| {
            state.select_space_source(space_id.clone(), member_ids.clone(), cx);
        });
        let target = {
            let state = self.state.read(cx);
            let is_member = |candidate: Option<&str>| {
                candidate.is_some_and(|candidate| member_ids.iter().any(|id| id == candidate))
            };
            let in_source = |id: &str| {
                state
                    .visible_chats()
                    .any(|chat| chat.id == id && is_member(chat.space_id.as_deref()))
            };
            member_ids
                .iter()
                .filter_map(|member_id| self.space_last_chat.get(member_id))
                .find(|id| in_source(id))
                .cloned()
                .or_else(|| {
                    state
                        .visible_chats()
                        .find(|chat| is_member(chat.space_id.as_deref()))
                        .map(|chat| chat.id.clone())
                })
        };
        self.state
            .update(cx, |state, cx| state.select_chat(target, cx));
        self.settings.last_space_id = Some(space_id);
        self.schedule_save(cx);
        cx.notify();
    }

    // ---- sidebar sections ----

    /// The "Folders" section: tracked header + add button, then a row per folder.
    pub(super) fn render_spaces_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A drag that ended off-list (no drop event) must not strand the
        // sibling slide offsets.
        if self.space_drag.is_some() && !cx.has_active_drag() {
            self.space_drag = None;
        }
        let (
            spaces,
            local_device_id,
            selected,
            device_names,
            offline_devices,
            attention,
            visible_space_ids,
        ) = {
            let now = Utc::now();
            let state = self.state.read(cx);
            let spaces = state.spaces.clone();
            let device_names: std::collections::HashMap<String, String> = spaces
                .iter()
                .map(|space| {
                    let name = if state.local_device_id.as_deref() == Some(space.device_id.as_str())
                    {
                        "This Mac".to_string()
                    } else {
                        folder_device_name(state.device_name(&space.device_id)).to_string()
                    };
                    (space.device_id.clone(), name)
                })
                .collect();
            // Host-presence (the revived "Remote" signal): a remote space whose
            // device heartbeat lapsed shows offline — a host outage, not slow sync.
            let offline_devices: std::collections::HashSet<String> = spaces
                .iter()
                .map(|s| s.device_id.clone())
                .filter(|id| {
                    state.local_device_id.as_deref() != Some(id.as_str())
                        && !state.device_online(id, now)
                })
                .collect();
            let visible_space_ids: std::collections::HashSet<String> = state
                .visible_chats()
                .filter_map(|chat| chat.space_id.clone())
                .collect();
            // Spaces with a live/awaiting session get an aggregate dot (the
            // most urgent member status wins) so the attention signal survives
            // even with the Sessions list scrolled off.
            let mut attention: std::collections::HashMap<String, ChatIndicator> =
                std::collections::HashMap::new();
            for chat in state.visible_chats() {
                let status = state.display_status_for(chat, now);
                if !matches!(
                    status,
                    ChatIndicator::Working | ChatIndicator::AwaitingInput
                ) {
                    continue;
                }
                let Some(space_id) = chat.space_id.clone() else {
                    continue;
                };
                attention
                    .entry(space_id)
                    .and_modify(|held| {
                        if crate::state::attention_rank(status)
                            < crate::state::attention_rank(*held)
                        {
                            *held = status;
                        }
                    })
                    .or_insert(status);
            }
            (
                spaces,
                state.local_device_id.clone(),
                state.selected_space.clone(),
                device_names,
                offline_devices,
                attention,
                visible_space_ids,
            )
        };
        let spaces = spaces_with_visible_sessions(
            spaces,
            &visible_space_ids,
            &self.settings.pinned_space_ids,
            selected.as_deref(),
        );
        // Manual (drag) order overrides the synced creation order — device-
        // local, resolved exactly like the session-tab order.
        let spaces: Vec<SidebarSource> = {
            let created: Vec<String> = spaces.iter().map(|s| s.id.clone()).collect();
            let order = super::tabs::resolve_tab_order(&created, &self.settings.space_order);
            let mut by_id: std::collections::HashMap<String, Space> =
                spaces.into_iter().map(|s| (s.id.clone(), s)).collect();
            let ordered = order.iter().filter_map(|id| by_id.remove(id)).collect();
            source_picker_spaces(ordered, local_device_id.as_deref())
        };

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(Theme::SPACE_SM))
            .pt(px(8.0))
            .pb(px(4.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("Folders")),
            )
            .child(
                div()
                    .id("add-space")
                    .size(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        "add-space",
                        crate::theme::wash(0.0),
                        crate::theme::wash(0.14),
                    ))
                    .on_hover(motion::hover_listener("add-space"))
                    .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx)))
                    .child(
                        icon(icons::PLUS)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    ),
            );

        let mut column = div().flex().flex_col().child(header);
        if spaces.is_empty() {
            // Ghost row: the empty-state affordance mirrors a space row.
            column = column.child(
                div()
                    .id("add-space-ghost")
                    .mx(px(0.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .rounded(px(8.0))
                    .px(px(Theme::SPACE_SM))
                    .py(px(6.0))
                    .text_size(px(13.0))
                    .text_color(motion::hover_blend(
                        "add-space-ghost",
                        theme.text_muted,
                        theme.text,
                    ))
                    .bg(motion::hover_blend(
                        "add-space-ghost",
                        theme.glass_hover().opacity(0.0),
                        theme.glass_hover(),
                    ))
                    .on_hover(motion::hover_listener("add-space-ghost"))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx)))
                    .child(
                        icon(icons::FOLDER)
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    )
                    .child(SharedString::from("Add folder")),
            );
        } else {
            let count = spaces.len();
            let drag = self
                .space_drag
                .as_ref()
                .map(|d| (d.from, d.over, d.epoch, d.prev_over));
            let rows: Vec<AnyElement> = spaces
                .into_iter()
                .enumerate()
                .map(|(ix, source)| {
                    let space = source.space;
                    let id = space.id.clone();
                    let device_name = device_names
                        .get(&space.device_id)
                        .cloned()
                        .unwrap_or_else(|| "Unknown device".to_string());
                    let host_offline = offline_devices.contains(&space.device_id);
                    let is_selected = selected
                        .as_deref()
                        .is_some_and(|id| source.member_ids.iter().any(|member| member == id));
                    let source_attention = source
                        .member_ids
                        .iter()
                        .filter_map(|id| attention.get(id).copied())
                        .min_by_key(|status| crate::state::attention_rank(*status));
                    let row = self.render_space_row(
                        ix,
                        space,
                        source.member_ids,
                        device_name,
                        host_offline,
                        is_selected,
                        source_attention,
                        theme,
                        cx,
                    );
                    // Sliding transform while a sibling is dragged over —
                    // the session-tab idiom, vertical.
                    match drag {
                        Some((from, over, epoch, prev_over)) if ix != from => {
                            let target = slide_offset(ix, from, over) * SPACE_ROW_SLOT;
                            let start = slide_offset(ix, from, prev_over) * SPACE_ROW_SLOT;
                            div()
                                .relative()
                                .child(row.with_animation(
                                    SharedString::from(format!("space-slide-{id}-{epoch}")),
                                    TAB_SLIDE.animation(),
                                    move |el, t| el.top(px(motion::lerp(start, target, t))),
                                ))
                                .into_any_element()
                        }
                        // The dragged row renders as an invisible spacer; the
                        // cursor ghost represents it.
                        Some((from, ..)) if ix == from => div()
                            .h(px(SPACE_ROW_SLOT - 2.0))
                            .flex_none()
                            .into_any_element(),
                        _ => row.into_any_element(),
                    }
                })
                .collect();
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .on_drag_move::<SpaceDragPayload>(cx.listener(
                        move |this, event: &gpui::DragMoveEvent<SpaceDragPayload>, _, cx| {
                            let from = event.drag(cx).from;
                            let rel_y =
                                f32::from(event.event.position.y) - f32::from(event.bounds.top());
                            let over = drop_index(rel_y, SPACE_ROW_SLOT, count);
                            this.update_space_drag_over(from, over, cx);
                        },
                    ))
                    .on_drop::<SpaceDragPayload>(cx.listener(
                        move |this, payload: &SpaceDragPayload, _, cx| {
                            let to = this
                                .space_drag
                                .as_ref()
                                .map(|d| d.over)
                                .unwrap_or(payload.from);
                            this.commit_space_reorder(payload.from, to, cx);
                        },
                    ))
                    .children(rows),
            );
        }
        column.into_any_element()
    }

    /// Track the drop slot while a space row is dragged over the list (150ms
    /// sibling slides restart per committed `over` change).
    fn update_space_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.space_drag {
            Some(drag) if drag.from == from => {
                if drag.over != over {
                    drag.prev_over = drag.over;
                    drag.over = over;
                    drag.epoch += 1;
                    cx.notify();
                }
            }
            _ => {
                self.space_drag = Some(SpaceDragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    /// Commit a drag: persist the new visual order (device-local).
    fn commit_space_reorder(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let (spaces, local_device_id, selected, visible_space_ids) = {
            let state = self.state.read(cx);
            (
                state.spaces.clone(),
                state.local_device_id.clone(),
                state.selected_space.clone(),
                state
                    .visible_chats()
                    .filter_map(|chat| chat.space_id.clone())
                    .collect(),
            )
        };
        let spaces = spaces_with_visible_sessions(
            spaces,
            &visible_space_ids,
            &self.settings.pinned_space_ids,
            selected.as_deref(),
        );
        let created: Vec<String> = spaces.iter().map(|space| space.id.clone()).collect();
        let resolved = super::tabs::resolve_tab_order(&created, &self.settings.space_order);
        let mut by_id: std::collections::HashMap<String, Space> = spaces
            .into_iter()
            .map(|space| (space.id.clone(), space))
            .collect();
        let ordered = resolved.iter().filter_map(|id| by_id.remove(id)).collect();
        let mut order: Vec<String> = source_picker_spaces(ordered, local_device_id.as_deref())
            .into_iter()
            .map(|source| source.space.id)
            .collect();
        if from < order.len() {
            reorder_tabs(&mut order, from, to);
            self.settings.space_order = order;
            self.schedule_save(cx);
        }
        self.space_drag = None;
        cx.notify();
    }

    /// One space row: folder icon + folder name, device name subline.
    /// `host_offline` marks a remote host whose presence heartbeat lapsed.
    #[allow(clippy::too_many_arguments)]
    fn render_space_row(
        &self,
        ix: usize,
        space: Space,
        member_ids: Vec<String>,
        device_name: String,
        host_offline: bool,
        selected: bool,
        attention: Option<ChatIndicator>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id = space.id.clone();
        let name: SharedString = space.display_name().to_string().into();
        let fade_key = format!("space-row-{id}");
        let rest_bg = if selected {
            crate::theme::glass_selected_bg()
        } else {
            crate::theme::wash(0.0)
        };
        let rest_text = if selected {
            theme.text
        } else {
            theme.text.opacity(0.8)
        };
        let select_id = id.clone();
        let menu_id = id.clone();
        // One line: "name @ device" — the folder name carries the weight, the
        // device tag rides along slightly muted. Long names truncate; the
        // device tag stays visible.
        div()
            .id(SharedString::from(format!("space-{id}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, theme.text))
            // Selected rows pin their hover target to the selected fill — see
            // the chat-row comment in shell.rs (light hover sits below the
            // near-opaque selected fill; blending toward it dims the row).
            .bg(motion::hover_blend(
                &fade_key,
                rest_bg,
                if selected {
                    rest_bg
                } else {
                    theme.glass_hover()
                },
            ))
            .when(selected, |el| {
                el.shadow(crate::theme::glass_selected_shadows())
            })
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.activate_source(select_id.clone(), member_ids.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.space_menu = Some((menu_id.clone(), event.position));
                    cx.notify();
                }),
            )
            .on_drag(
                SpaceDragPayload {
                    from: ix,
                    name: name.clone(),
                },
                |payload, _point, _, cx| {
                    let name = payload.name.clone();
                    cx.stop_propagation();
                    cx.new(|_| SpaceGhost { name })
                },
            )
            // Status dot LEADS the row (like session rows) so its position is
            // stable — appearing/disappearing at the right edge made the row
            // jitter (user request). Faint at rest, colored under attention.
            .child(
                div().size(px(6.0)).rounded_full().flex_none().bg(attention
                    .map(|status| status_dot_color(status, theme))
                    .unwrap_or_else(|| crate::theme::ink(0.14))),
            )
            .child(
                icon(icons::FOLDER)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .line_height(px(17.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(name),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.0))
                    .line_height(px(17.0))
                    .text_color(if host_offline {
                        theme.warning.opacity(0.8)
                    } else {
                        theme.text_muted.opacity(0.6)
                    })
                    .child(SharedString::from(if host_offline {
                        format!("{device_name} · Offline")
                    } else {
                        device_name
                    })),
            )
    }

    /// The global "Sessions" list: every active session across all spaces
    /// (idle included), newest first. Rows are keyed for the FLIP resort glide.
    pub(super) fn render_active_rows(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<(String, f32, AnyElement)> {
        let now = Utc::now();
        let chats = {
            let state = self.state.read(cx);
            state
                .overview_chats(now)
                .into_iter()
                .map(|(status, chat)| (status, chat.clone()))
                .collect()
        };
        self.render_session_rows(chats, false, theme, cx)
    }

    /// Archived sessions remain immediately reachable below the active list.
    pub(super) fn render_settled_rows(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<(String, f32, AnyElement)> {
        let chats = {
            let state = self.state.read(cx);
            state
                .settled_chats()
                .into_iter()
                .map(|chat| (ChatIndicator::Idle, chat.clone()))
                .collect()
        };
        self.render_session_rows(chats, true, theme, cx)
    }

    fn render_session_rows(
        &mut self,
        chats: Vec<(ChatIndicator, comet_proto::Chat)>,
        settled: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<(String, f32, AnyElement)> {
        let now = Utc::now();
        let rows: Vec<(
            ChatIndicator,
            comet_proto::Chat,
            String,
            Option<String>,
            String,
            super::SidebarSessionMeta,
        )> = {
            let state = self.state.read(cx);
            chats
                .into_iter()
                .map(|(status, chat)| {
                    let status = if state.scaffold_chat_starting(&chat.id) {
                        ChatIndicator::Working
                    } else {
                        status
                    };
                    let space = state.space_for_chat(&chat);
                    let mut folder = space
                        .map(|space| space.display_name().to_string())
                        .unwrap_or_else(|| "?".to_string());
                    if state.local_device_id.as_deref() != Some(chat.device_id.as_str())
                        && let Some(device) = state.device_name(&chat.device_id)
                    {
                        folder = format!("{folder} · {device}");
                    }
                    let branch = sidebar_branch_label(&chat);
                    let scaffold_environment = state.scaffold_environment(&chat.id);
                    let scaffold_title =
                        scaffold_environment.and_then(|environment| environment.name.clone());
                    let (scaffold_web, scaffold_ide) = scaffold_environment
                        .and_then(|environment| match &environment.source {
                            comet_proto::SessionEnvironmentSource::Scaffold { links, .. } => {
                                Some((links.web.clone(), links.opencode.clone()))
                            }
                            comet_proto::SessionEnvironmentSource::Local => None,
                        })
                        .unwrap_or_default();
                    let title = scaffold_title
                        .or_else(|| chat.title.clone())
                        .unwrap_or_else(|| "New session".into());
                    let agent_session = state.collaboration_sessions(&chat.id).next();
                    let source = sidebar_session_source(
                        state.local_device_id.as_deref(),
                        &chat.device_id,
                        state.chat_is_scaffold(&chat.id),
                        agent_session.map(|session| session.source),
                    );
                    let runtime = chat
                        .config
                        .as_ref()
                        .map(|config| crate::multiplayer::harness_label(config.harness));
                    let model = agent_session
                        .and_then(|session| session.model.as_deref())
                        .or_else(|| {
                            chat.config
                                .as_ref()
                                .and_then(|config| config.model.as_deref())
                        });
                    let runtime_model = crate::multiplayer::runtime_model(runtime, model).into();
                    (
                        status,
                        chat,
                        folder,
                        branch,
                        title,
                        super::SidebarSessionMeta {
                            source,
                            runtime_model,
                            scaffold_web: scaffold_web.map(SharedString::from),
                            scaffold_ide: scaffold_ide.map(SharedString::from),
                        },
                    )
                })
                .collect()
        };
        let selected = self.state.read(cx).selected_chat.clone();
        rows.into_iter()
            .map(|(status, chat, folder, branch, title, meta)| {
                let time_ago: SharedString =
                    format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now).into();
                let is_selected = selected.as_deref() == Some(chat.id.as_str());
                let height = super::chat_row_height(self.settings.density);
                let element = self.render_chat_row(
                    chat.id.clone(),
                    transcript::single_line(&title).into(),
                    time_ago,
                    folder.into(),
                    branch.map(SharedString::from),
                    meta,
                    status,
                    settled,
                    is_selected,
                    theme,
                    cx,
                );
                let element = if settled {
                    div().opacity(0.52).child(element).into_any_element()
                } else {
                    element
                };
                (
                    format!("{}:{}", if settled { "s" } else { "c" }, chat.id),
                    height,
                    element,
                )
            })
            .collect()
    }

    // ---- add-space flow (the ⌘K palette) ----

    pub(super) fn open_add_space(&mut self, cx: &mut Context<Self>) {
        let devices: Vec<Device> = self.state.read(cx).devices.clone();
        let local = self.state.read(cx).local_device_id.clone();
        // Land on this device's tab (else the first registered device).
        let device = devices
            .iter()
            .find(|d| local.as_deref() == Some(d.id.as_str()))
            .or_else(|| devices.first())
            .cloned();
        // "PaletteSearch" context: navigation keys stay unbound so ↑↓/←/→/⏎
        // bubble to the palette frame (`add_space_key`) instead of moving the
        // text caret — Enter and ⌘Enter are both handled there.
        let search =
            cx.new(|cx| ComposerInput::with_context("Search folders…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                if let Some(flow) = this.add_space.as_mut() {
                    flow.active = 0;
                }
                cx.notify();
            }
        });
        let has_device = device.is_some();
        self.add_space = Some(AddSpaceFlow {
            device,
            search,
            browser: Loadable::Idle,
            browser_path: None,
            home: None,
            browser_repo: false,
            active: 0,
            submit_busy: false,
            error: None,
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            focus_pending: true,
            load_task: None,
            submit_task: None,
            _search_events: search_events,
        });
        if has_device {
            self.load_space_folders(None, cx);
        }
        cx.notify();
    }

    /// Devices-rail click: rebrowse the same palette on another device.
    fn add_space_pick_device(&mut self, device: Device, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if flow.device.as_ref().is_some_and(|d| d.id == device.id) {
            return;
        }
        flow.device = Some(device);
        flow.browser = Loadable::Idle;
        flow.browser_path = None;
        flow.home = None;
        flow.browser_repo = false;
        flow.active = 0;
        flow.error = None;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(None, cx);
        cx.notify();
    }

    /// The current listing's folder rows filtered by the search query
    /// (prefix matches first — `popover::filter_indices`).
    fn add_space_filtered(&self, cx: &App) -> Vec<comet_proto::FolderEntry> {
        let Some(flow) = self.add_space.as_ref() else {
            return Vec::new();
        };
        let Some(listing) = flow.browser.ready() else {
            return Vec::new();
        };
        let dirs = browser_rows(listing);
        let query = flow.search.read(cx).text().to_string();
        let names: Vec<&str> = dirs.iter().map(|e| e.name.as_str()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| dirs[ix].clone())
            .collect()
    }

    /// Descend into the highlighted (filtered) folder; clears the query.
    fn add_space_open_active(&mut self, cx: &mut Context<Self>) {
        let rows = self.add_space_filtered(cx);
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let Some(entry) = rows.get(flow.active) else {
            return;
        };
        let full = crate::pickers::child_path(&listing.path, &entry.name);
        let is_repo = entry.is_repo;
        let search = flow.search.clone();
        if let Some(flow) = self.add_space.as_mut() {
            flow.browser_repo = is_repo;
        }
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
    }

    /// Descend into a specific folder row (mouse path); clears the query.
    fn add_space_descend(&mut self, full: String, is_repo: bool, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.browser_repo = is_repo;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
    }

    /// ListFolders on the flow's device (relay-forwarded when remote).
    pub(super) fn load_space_folders(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        let device_id = flow.device.as_ref().map(|d| d.id.clone());
        let went_home = path.is_none();
        flow.browser_path = path.clone();
        flow.browser = Loadable::Loading;
        flow.active = 0;
        flow.list_scroll.set_offset(gpui::Point::default());
        flow.load_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            if let Some(p) = &path {
                params.insert("path".into(), serde_json::Value::String(p.clone()));
            }
            // Only target remote devices — local calls skip the relay.
            if let (Some(target), local) = (&device_id, &local)
                && local.as_deref() != Some(target.as_str())
            {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_FOLDERS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(flow) = shell.add_space.as_mut() {
                    flow.browser = match result {
                        Ok(value) => match serde_json::from_value::<FolderListing>(value) {
                            Ok(listing) => {
                                // A pathless browse resolved home — remember it
                                // so the breadcrumbs can fold it into the
                                // device crumb.
                                if went_home {
                                    flow.home = Some(listing.path.clone());
                                }
                                Loadable::Ready(listing)
                            }
                            Err(err) => Loadable::Error(err.to_string()),
                        },
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn retain_space_in_sidebar(&mut self, space_id: &str, cx: &mut Context<Self>) {
        if self
            .settings
            .pinned_space_ids
            .iter()
            .any(|id| id == space_id)
        {
            return;
        }
        self.settings.pinned_space_ids.push(space_id.to_string());
        self.schedule_save(cx);
    }

    /// Create the space for the browser's current folder.
    fn submit_add_space(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        if flow.submit_busy {
            return;
        }
        let Some(device) = flow.device.clone() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let path = listing.path.clone();
        let git_detected = flow.browser_repo;
        // Same (device, folder) already has a space → just switch to it. The
        // engine dedupes this case too (a createSpace for a duplicate pair
        // no-ops), so creating would leave the minted id dangling.
        if let Some(existing) = self
            .state
            .read(cx)
            .spaces
            .iter()
            .find(|s| s.device_id == device.id && s.path == path)
            .map(|s| s.id.clone())
        {
            self.add_space = None;
            self.retain_space_in_sidebar(&existing, cx);
            self.activate_space(existing, cx);
            return;
        }
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.submit_busy = true;
        flow.error = None;
        let space_id = uuid::Uuid::new_v4().to_string();
        // Optimistic echo: the watch frame carrying the real row replaces it
        // by id (apply_spaces re-sorts; same-id upsert is idempotent).
        let space = Space {
            id: space_id.clone(),
            device_id: device.id.clone(),
            path: path.clone(),
            name: None,
            git_detected,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        self.state.update(cx, |s, cx| {
            if !s.spaces.iter().any(|existing| existing.id == space.id) {
                s.spaces.push(space);
            }
            cx.notify();
        });
        let params = serde_json::json!({
            "op": "createSpace",
            "spaceId": space_id,
            "deviceId": device.id,
            "path": path,
            "gitDetected": git_detected,
        });
        let submit_id = space_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => {
                        shell.add_space = None;
                        shell.retain_space_in_sidebar(&submit_id, cx);
                        shell.activate_space(submit_id.clone(), cx);
                    }
                    Err(err) => {
                        // Roll the optimistic row back; surface the error inline.
                        shell.state.update(cx, |s, cx| {
                            s.spaces.retain(|space| space.id != submit_id);
                            cx.notify();
                        });
                        if let Some(flow) = shell.add_space.as_mut() {
                            flow.submit_busy = false;
                            flow.error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(flow) = self.add_space.as_mut() {
            flow.submit_task = Some(task);
        }
        cx.notify();
    }

    /// Go up to the parent folder (←, and ⌫ on an empty query).
    fn add_space_go_up(&mut self, cx: &mut Context<Self>) {
        let parent = self
            .add_space
            .as_ref()
            .and_then(|f| f.browser.ready())
            .and_then(|l| parent_path(&l.path));
        if let Some(parent) = parent {
            if let Some(flow) = self.add_space.as_mut() {
                flow.browser_repo = false; // unknown at the parent
            }
            self.load_space_folders(Some(parent), cx);
        }
    }

    /// Palette keys (bubbling from the focused search input) — every legend
    /// maps to a REAL key: ↑↓ navigate, →/⏎ open the highlighted folder,
    /// ← up a level, ⌘⏎ add the OPEN folder, ⌫ (empty query) also goes up,
    /// esc closes.
    fn add_space_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        // ←/→ act on the FOLDERS, not the text cursor — the palette is a
        // navigator first; queries are short and edited with ⌫.
        match event.keystroke.key.as_str() {
            "right" => {
                self.add_space_open_active(cx);
                return;
            }
            "left" => {
                self.add_space_go_up(cx);
                return;
            }
            _ => {}
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.add_space = None;
                cx.notify();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.add_space_filtered(cx).len();
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(flow) = self.add_space.as_mut() {
                    flow.active = popover::menu_step(Some(flow.active), count, delta).unwrap_or(0);
                    // Keep the highlighted row in view as the cursor walks
                    // past the viewport (user-reported: the list didn't
                    // follow the keyboard).
                    flow.list_scroll.scroll_to_item(flow.active);
                    cx.notify();
                }
            }
            // ⏎ opens the highlighted folder (an alias for →); the space is
            // added with ⌘⏎ — and the chord acts on the folder OPEN in the
            // breadcrumbs, not the highlight. The highlight auto-rests on the
            // first row, so a chord that took it would add arbitrary
            // subfolders; the usual target (a repo root full of subfolders)
            // is only ever "the folder you're standing in".
            popover::MenuKey::Enter => self.add_space_open_active(cx),
            popover::MenuKey::ModEnter => self.submit_add_space(cx),
            popover::MenuKey::Backspace => {
                let empty = self
                    .add_space
                    .as_ref()
                    .is_some_and(|f| f.search.read(cx).is_empty());
                if empty {
                    self.add_space_go_up(cx);
                }
            }
            popover::MenuKey::Other => {}
        }
    }

    /// The palette card: ⌘K search bar (with the ⌘⏎ add / esc chips) ·
    /// breadcrumbs + folder list beside the devices rail · kbd-hint footer.
    pub(super) fn render_add_space_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        {
            let flow = self.add_space.as_mut()?;
            if std::mem::take(&mut flow.focus_pending) {
                let handle = flow.search.focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        let (
            device,
            search,
            error,
            submit_busy,
            active,
            loading,
            load_error,
            listing,
            focus,
            list_scroll,
            home,
        ) = {
            let flow = self.add_space.as_ref()?;
            (
                flow.device.clone(),
                flow.search.clone(),
                flow.error.clone(),
                flow.submit_busy,
                flow.active,
                matches!(flow.browser, Loadable::Loading | Loadable::Idle),
                flow.browser.error().map(str::to_string),
                flow.browser.ready().cloned(),
                flow.focus.clone(),
                flow.list_scroll.clone(),
                flow.home.clone(),
            )
        };
        let devices = self.state.read(cx).devices.clone();
        let rows = self.add_space_filtered(cx);
        let query_empty = search.read(cx).is_empty();
        let hairline = crate::theme::hairline(0.06);
        let now = Utc::now();
        // (browsed device name, online) per rail row — presence is the same
        // signal the sidebar space rows use.
        let device_presence: Vec<bool> = {
            let state = self.state.read(cx);
            devices
                .iter()
                .map(|d| state.device_online(&d.id, now))
                .collect()
        };
        let device_name: SharedString = device
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "This device".to_string())
            .into();

        // A quiet mono key-cap chip ("⌘K" / "esc") for the search bar ends.
        let key_chip = |theme: &Theme| {
            div()
                .h(px(22.0))
                .px(px(6.0))
                .rounded(px(5.0))
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .bg(crate::theme::ink(0.05))
                .text_size(px(11.0))
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_muted.opacity(0.7))
        };

        // ── search bar (the ⌘K bar): summon chip · input · "⌘ Enter" add ·
        //    esc. The primary chip leads with the ⌘ glyph, then says "Enter"
        //    in words (user request — the bare return arrow read as noise).
        let submit_chip = popover::btn_primary(&theme, "")
            .id("add-space-submit")
            .h(px(22.0))
            .px(px(8.0))
            .py(px(0.0))
            // Match the key-cap chips beside it (rounded-5) — btn_primary's
            // rounded-8 at this size read as a different component.
            .rounded(px(5.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .text_size(px(12.0))
            .when(submit_busy || listing.is_none(), |el| el.opacity(0.6))
            .on_click(cx.listener(|this, _, _, cx| this.submit_add_space(cx)))
            .when(!submit_busy, |el| {
                el.child(
                    icon(icons::COMMAND)
                        .size(px(11.0))
                        .text_color(theme.on_solid.opacity(0.8)),
                )
                .child(SharedString::from("Enter"))
            })
            .when(submit_busy, |el| el.child(SharedString::from("Adding…")));
        // Header and footer sit a shade DEEPER than the body (the shared
        // recessed-band tone) — the bands frame the folder list, which stays
        // on the brighter tint.
        let band = popover::band();
        let input_row = div()
            .h(px(46.0))
            .flex_none()
            .pl(px(12.0))
            .pr(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .bg(band)
            .border_b_1()
            .border_color(hairline)
            .child(
                key_chip(&theme)
                    .child(
                        icon(icons::COMMAND)
                            .size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    )
                    .child(SharedString::from("K")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(14.0))
                    .child(search.clone().into_any_element()),
            )
            .child(submit_chip)
            .child(
                key_chip(&theme)
                    .id("add-space-esc")
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::ink(0.09)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.add_space = None;
                        cx.notify();
                    }))
                    .child(SharedString::from("esc")),
            );

        // ── breadcrumbs ("MacBook Pro / Projects / comet"): the quiet mono
        //    path voice, `/` separators. The device crumb stands in for home —
        //    everything up to the resolved home path folds into it; below
        //    home the full path shows. Ancestors (device crumb included) are
        //    clickable.
        let crumbs: AnyElement = match &listing {
            Some(listing) => {
                let segments = breadcrumbs(&listing.path);
                let last = segments.len().saturating_sub(1);
                // Root "/" chip always folds; the home segments fold too when
                // the browsed path sits at/under home.
                let at_home = home.as_deref() == Some(listing.path.as_str());
                let folded = 1 + home
                    .as_deref()
                    .filter(|h| listing.path == *h || listing.path.starts_with(&format!("{h}/")))
                    .map(|h| h.split('/').filter(|s| !s.is_empty()).count())
                    .unwrap_or(0);
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .px(px(13.0))
                    .pt(px(10.0))
                    .pb(px(2.0))
                    .text_size(px(11.0))
                    .font_family(theme.font_mono.clone())
                    .child({
                        let crumb = div()
                            .id("add-space-crumb-device")
                            .px(px(3.0))
                            .rounded(px(4.0))
                            .child(device_name.clone());
                        if at_home {
                            // Standing at home — the device crumb IS the
                            // current folder.
                            crumb
                                .text_color(theme.text.opacity(0.85))
                                .into_any_element()
                        } else {
                            crumb
                                .text_color(theme.text_muted.opacity(0.55))
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme.text))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(flow) = this.add_space.as_mut() {
                                        flow.browser_repo = false;
                                    }
                                    this.load_space_folders(None, cx);
                                }))
                                .into_any_element()
                        }
                    })
                    .children(segments.into_iter().enumerate().skip(folded).map(
                        |(ix, (label, full))| {
                            let is_last = ix == last;
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .child(
                                    div()
                                        .text_color(theme.text_faint.opacity(0.7))
                                        .child(SharedString::from("/")),
                                )
                                .child({
                                    let crumb = div()
                                        .id(("add-space-crumb", ix))
                                        .px(px(3.0))
                                        .rounded(px(4.0))
                                        .text_color(if is_last {
                                            theme.text.opacity(0.85)
                                        } else {
                                            theme.text_muted.opacity(0.55)
                                        })
                                        .child(SharedString::from(label));
                                    if is_last {
                                        crumb.into_any_element()
                                    } else {
                                        crumb
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(theme.text))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Some(flow) = this.add_space.as_mut() {
                                                    flow.browser_repo = false;
                                                }
                                                this.load_space_folders(Some(full.clone()), cx);
                                            }))
                                            .into_any_element()
                                    }
                                })
                        },
                    ))
                    .into_any_element()
            }
            None => div().pt(px(6.0)).into_any_element(),
        };

        // ── folder list ─────────────────────────────────────────────────────
        let base_path = listing.as_ref().map(|l| l.path.clone()).unwrap_or_default();
        let list: AnyElement = if loading {
            div()
                .px(px(8.0))
                .py(px(6.0))
                .child(popover::skeleton_rows(
                    "add-space-skeleton",
                    &theme,
                    6,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element()
        } else if let Some(message) = load_error {
            let device_line = device
                .as_ref()
                .map(|d| format!("{} didn't respond — is it online?", d.name))
                .unwrap_or(message);
            popover::error_row(&theme, &device_line)
                .px(px(14.0))
                .py(px(10.0))
                .child(
                    div()
                        .id("add-space-retry")
                        .px(px(Theme::SPACE_SM))
                        .py(px(3.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.element_hover))
                        .on_click(cx.listener(|this, _, _, cx| {
                            let path = this.add_space.as_ref().and_then(|f| f.browser_path.clone());
                            this.load_space_folders(path, cx);
                        }))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element()
        } else if rows.is_empty() {
            div()
                .px(px(14.0))
                .py(px(16.0))
                .text_size(px(12.5))
                .text_color(theme.text_faint)
                .child(SharedString::from(if query_empty {
                    "No folders here"
                } else {
                    "No folders match"
                }))
                .into_any_element()
        } else {
            // The 6px gutters live on a WRAPPER, outside the scroll viewport:
            // in-content padding/spacers can't do it — the wheel's max offset
            // eats bottom padding, and `scroll_to_item` (keyboard) pins the
            // row's bottom to the viewport edge regardless.
            div()
                .flex_1()
                .min_h_0()
                .py(px(6.0))
                .child(
                    div()
                        .id("add-space-folders")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&list_scroll)
                        .px(px(8.0))
                        .flex()
                        .flex_col()
                        // The app-wide list rhythm (sidebar rows, menu rows): 2px.
                        .gap(px(2.0))
                        .children(rows.into_iter().enumerate().map(|(ix, entry)| {
                            let name: SharedString = entry.name.clone().into();
                            let full = crate::pickers::child_path(&base_path, &entry.name);
                            let is_repo = entry.is_repo;
                            popover::menu_row_nav(
                                &theme,
                                false,
                                ix == active,
                                format!("add-space-folder-{ix}"),
                            )
                            // The floating-card selection language: the wash
                            // plus the ring-only inset outline.
                            .when(ix == active, |el| {
                                el.shadow(crate::theme::card_selected_shadows())
                            })
                            .id(("add-space-folder", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.add_space_descend(full.clone(), is_repo, cx);
                            }))
                            .child(
                                icon(icons::FOLDER)
                                    .size(px(15.0))
                                    .flex_none()
                                    .text_color(theme.text_muted.opacity(0.8)),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(name))
                            // Repos get a quiet trailing branch glyph — the row
                            // you're usually hunting for announces itself.
                            .when(is_repo, |el| {
                                el.child(
                                    icon(icons::GIT_BRANCH)
                                        .size(px(13.0))
                                        .flex_none()
                                        .text_color(theme.text_muted.opacity(0.5)),
                                )
                            })
                        })),
                )
                .into_any_element()
        };

        // ── devices rail (mock right column): platform glyph + name +
        //    presence dot per row, an info line naming the browsed device.
        //    Rows are the tab recipe (h-28 rounded-8 washes), vertical.
        let rail = div()
            .w(px(196.0))
            .flex_none()
            .border_l_1()
            .border_color(hairline)
            .px(px(8.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(2.0))
                    .pb(px(4.0))
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("Devices")),
            )
            .children(devices.into_iter().enumerate().map(|(ix, dev)| {
                let is_active = device.as_ref().is_some_and(|d| d.id == dev.id);
                let online = device_presence.get(ix).copied().unwrap_or(false);
                // The Devices-page platform mapping (settings::devices).
                let platform_icon = match dev.platform.as_str() {
                    "macos" | "darwin" => icons::LAPTOP,
                    "web" => icons::GLOBAL,
                    "ios" | "android" => icons::SMARTPHONE,
                    _ => icons::MONITOR,
                };
                let name: SharedString = dev.name.clone().into();
                let pick = dev.clone();
                div()
                    .id(("add-space-device", ix))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(12.5))
                    .cursor_pointer()
                    .when(is_active, |el| {
                        // The floating-card selection language: wash +
                        // ring-only inset outline.
                        el.bg(crate::theme::card_selected_bg())
                            .shadow(crate::theme::card_selected_shadows())
                            .text_color(theme.text)
                    })
                    .when(!is_active, |el| {
                        el.text_color(theme.text_muted.opacity(0.7))
                            .hover(|s| s.bg(theme.element_hover))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_space_pick_device(pick.clone(), cx);
                    }))
                    .child(
                        icon(platform_icon)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(theme.text_muted.opacity(0.8)),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .child(
                        div()
                            .size(px(5.0))
                            .rounded_full()
                            .flex_none()
                            .when(online, |el| {
                                // The Devices-page presence emerald, soft glow
                                // included.
                                let emerald = theme.success;
                                el.bg(emerald.opacity(0.9)).shadow(vec![gpui::BoxShadow {
                                    color: emerald.opacity(0.55),
                                    offset: gpui::point(px(0.0), px(0.0)),
                                    blur_radius: px(6.0),
                                    spread_radius: px(0.0),
                                    inset: false,
                                }])
                            })
                            .when(!online, |el| el.bg(crate::theme::ink(0.22))),
                    )
            }))
            .child(div().h(px(1.0)).mx(px(2.0)).my(px(6.0)).bg(hairline))
            .child(
                div()
                    .px(px(8.0))
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(6.0))
                    .text_size(px(11.0))
                    .line_height(px(15.0))
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(
                        icon(icons::INFO_CIRCLE)
                            .size(px(12.0))
                            .flex_none()
                            .mt(px(1.0))
                            .text_color(theme.text_muted.opacity(0.5)),
                    )
                    .child(div().min_w_0().child(SharedString::from(format!(
                        "Showing folders from {device_name} only"
                    )))),
            );

        // ── body: folder column (crumbs + list) beside the devices rail.
        //    FIXED height — sparse folders, loading skeletons, and device
        //    switches must not resize the card (the list fills and scrolls).
        let body = div()
            .h(px(330.0))
            .flex()
            .flex_row()
            .items_stretch()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(crumbs)
                    .child(list),
            )
            .child(rail);

        // ── footer: the shared key-cap legend voice (popover::key_hint).
        let footer = div()
            .flex_none()
            .bg(band)
            .border_t_1()
            .border_color(hairline)
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(popover::key_hint_pair(
                &theme,
                icons::ARROW_UP,
                icons::ARROW_DOWN,
                "Navigate",
            ))
            .child(popover::key_hint(&theme, icons::ARROW_LEFT, "Up"))
            .child(popover::key_hint(&theme, icons::ARROW_RIGHT, "Open"))
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .child(message),
                )
            });

        let card =
            div()
                .id("add-space-palette")
                .w(px(680.0))
                .rounded(px(14.0))
                .border_1()
                .border_color(crate::theme::hairline(0.10))
                // The popover_card glass recipe: a translucent tint over the
                // frosted backdrop blur (`popover::modal` wraps in `frosted`) —
                // an opaque fill here killed the vibrancy every other float has.
                .bg(if theme.is_glass() {
                    theme.glass_overlay()
                } else {
                    theme.surface_overlay
                })
                .shadow_lg()
                .overflow_hidden()
                .flex()
                .flex_col()
                .text_color(theme.text)
                // On the keyboard dispatch path (see `AddSpaceFlow::focus`) — the
                // pickers' proven structure for frame-level keys with a focused
                // child input.
                .track_focus(&focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    this.add_space_key(event, cx)
                }))
                // Clicking the scrim dismisses (user requirement) — same close
                // path as Escape.
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.add_space = None;
                    cx.notify();
                }))
                .child(input_row)
                .child(body)
                .child(footer)
                .into_any_element();
        Some(popover::modal("add-space-dialog", viewport, card))
    }

    // ---- space context menu / rename / delete overlays ----

    pub(super) fn open_rename_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.space_menu = None;
        let current = self
            .state
            .read(cx)
            .space_row(&space_id)
            .map(|s| s.display_name().to_string())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Space name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_space(cx);
            }
        });
        self.rename_space_dialog = Some(RenameSpaceDialog {
            space_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn submit_rename_space(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_space_dialog.take() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if !name.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameSpace", "spaceId": dialog.space_id, "name": name }),
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn delete_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.delete_space_confirm = None;
        self.mutate(
            serde_json::json!({ "op": "deleteSpace", "spaceId": space_id }),
            cx,
        );
        cx.notify();
    }

    /// Space context menu + rename dialog + delete confirm (appended to the
    /// shell's overlay list).
    pub(super) fn render_space_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((space_id, position)) = self.space_menu.clone() {
            let rename_id = space_id.clone();
            let delete_id = space_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(170.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.space_menu = None;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-rename-{space_id}"))
                        .id("space-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_space(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                )
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-delete-{space_id}"))
                        .id("space-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.space_menu = None;
                            this.delete_space_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Remove…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at("space-context-menu", position, menu));
        }

        if let Some(dialog) = &mut self.rename_space_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_space_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename folder"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "rename-space-cancel")
                                .id("rename-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_space_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-space-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_space(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-space-dialog", viewport, card));
        }

        if let Some(space_id) = self.delete_space_confirm.clone() {
            let (device, count) = {
                let state = self.state.read(cx);
                let space = state.space_row(&space_id);
                (
                    space
                        .and_then(|space| state.device_name(&space.device_id))
                        .unwrap_or("its device")
                        .to_string(),
                    state.chats_in_space(&space_id).len(),
                )
            };
            let copy = if count == 1 {
                format!("This permanently deletes its 1 session on {device}.")
            } else {
                format!("This permanently deletes its {count} sessions on {device}.")
            };
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Remove folder?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, copy)))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-space-cancel")
                                .id("delete-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_space_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Remove")
                                .id("delete-space-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_space(space_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-space-dialog", viewport, card));
        }

        overlays
    }
}

#[cfg(test)]
mod tests {
    use super::{
        collapse_sidebar_sources, detached_worktree_label, folder_device_name,
        sidebar_session_source, source_picker_spaces, spaces_with_visible_sessions,
    };
    use chrono::{DateTime, Utc};
    use comet_proto::{AgentSessionSource, Space};
    use std::path::PathBuf;

    fn space(id: &str, device_id: &str, path: &str, created_at: i64) -> Space {
        Space {
            id: id.into(),
            device_id: device_id.into(),
            path: path.into(),
            name: None,
            git_detected: true,
            git_checked_at: None,
            checkout_id: None,
            created_at: DateTime::<Utc>::from_timestamp(created_at, 0).unwrap(),
        }
    }
    #[test]
    fn sidebar_hides_stale_spaces_but_keeps_selected_and_manually_added_folders() {
        let spaces = vec![
            space("active", "device-current", "/repo/active", 1),
            space("manual", "device-current", "/repo/manual", 2),
            space("selected", "device-current", "/repo/selected", 3),
            space("stale", "device-current", "/repo/stale", 4),
        ];
        let visible = std::collections::HashSet::from(["active".to_string()]);
        let retained = vec!["manual".to_string()];

        let filtered = spaces_with_visible_sessions(spaces, &visible, &retained, Some("selected"));

        assert_eq!(
            filtered
                .iter()
                .map(|space| space.id.as_str())
                .collect::<Vec<_>>(),
            ["active", "manual", "selected"]
        );
    }

    #[test]
    fn staged_scaffold_session_labels_a_local_chat_remote() {
        assert_eq!(
            sidebar_session_source(Some("device-current"), "device-current", true, None),
            AgentSessionSource::Scaffold
        );
        assert_eq!(
            sidebar_session_source(Some("device-current"), "device-current", false, None),
            AgentSessionSource::Local
        );
    }

    #[test]
    fn manual_reorder_does_not_retain_a_stale_space() {
        let spaces = vec![
            space("active", "device-current", "/repo/active", 1),
            space("stale", "device-current", "/repo/stale", 2),
        ];
        let visible = std::collections::HashSet::from(["active".to_string()]);
        let settings = crate::settings::UiSettings {
            space_order: vec!["stale".to_string(), "active".to_string()],
            ..Default::default()
        };

        let filtered = spaces_with_visible_sessions(
            spaces,
            &visible,
            &settings.pinned_space_ids,
            Some("active"),
        );

        assert_eq!(
            filtered
                .iter()
                .map(|space| space.id.as_str())
                .collect::<Vec<_>>(),
            ["active"]
        );
    }

    #[test]
    fn worktrees_from_one_repository_collapse_and_keep_all_member_ids() {
        let spaces = vec![
            space("main", "device-current", "/repos/comet", 1),
            space("worktree-old", "device-current", "/tmp/comet-old", 2),
            space("worktree-new", "device-current", "/tmp/comet-new", 4),
        ];

        let sources = collapse_sidebar_sources(spaces.clone(), Some("device-current"), |_| {
            Some(PathBuf::from("/repos/comet/.git"))
        });

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].space.id, "worktree-new");
        let member_ids: Vec<&str> = sources[0].member_ids.iter().map(String::as_str).collect();
        assert_eq!(member_ids, ["main", "worktree-new", "worktree-old"]);
        assert_eq!(spaces.len(), 3, "the shared projection remains untouched");
    }

    #[test]
    fn deleted_agent_worktrees_collapse_into_their_main_repository() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let main = project.to_string_lossy().into_owned();
        let codex = temp
            .path()
            .join(".codex/worktrees/1de4/project")
            .to_string_lossy()
            .into_owned();
        let t3 = temp
            .path()
            .join("launcher/.t3/worktrees/project/t3code-deadbeef")
            .to_string_lossy()
            .into_owned();
        let claude = project
            .join(".claude/worktrees/restyle")
            .to_string_lossy()
            .into_owned();

        let sources = source_picker_spaces(
            vec![
                space("main", "device-current", &main, 1),
                space("codex", "device-current", &codex, 2),
                space("t3", "device-current", &t3, 3),
                space("claude", "device-current", &claude, 4),
            ],
            Some("device-current"),
        );

        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0]
                .member_ids
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["claude", "codex", "main", "t3"]
        );
        assert_eq!(sources[0].space.id, "main");
    }

    #[test]
    fn same_basename_from_different_repositories_stays_separate() {
        let spaces = vec![
            space("repo-a", "device-current", "/one/project", 1),
            space("repo-b", "device-current", "/two/project", 2),
        ];

        let sources = collapse_sidebar_sources(spaces, Some("device-current"), |space| {
            Some(PathBuf::from(format!("{}/.git", space.path)))
        });
        let ids: Vec<&str> = sources
            .iter()
            .map(|source| source.space.id.as_str())
            .collect();

        assert_eq!(ids, ["repo-a", "repo-b"]);
    }

    #[test]
    fn remote_device_sources_are_never_merged() {
        let spaces = vec![
            space("remote-a", "device-a", "/shared/project", 1),
            space("remote-b", "device-b", "/shared/project", 2),
            space("local", "device-current", "/shared/project", 3),
        ];

        let sources = collapse_sidebar_sources(spaces, Some("device-current"), |_| {
            Some(PathBuf::from("/shared/project/.git"))
        });
        let ids: Vec<&str> = sources
            .iter()
            .map(|source| source.space.id.as_str())
            .collect();

        assert_eq!(ids, ["remote-a", "remote-b", "local"]);
    }

    #[test]
    fn source_order_and_representatives_are_deterministic() {
        let spaces = vec![
            space("repo-b-old", "device-current", "/b/old", 1),
            space("repo-a", "device-current", "/a", 2),
            space("repo-b-z", "device-current", "/b/z", 4),
            space("repo-b-a", "device-current", "/b/a", 4),
            space("plain", "device-current", "/plain", 5),
        ];
        let sources = collapse_sidebar_sources(spaces, Some("device-current"), |space| {
            space
                .id
                .starts_with("repo-b")
                .then(|| PathBuf::from("/b/.git"))
        });
        let ids: Vec<&str> = sources
            .iter()
            .map(|source| source.space.id.as_str())
            .collect();

        assert_eq!(ids, ["repo-b-a", "repo-a", "plain"]);
    }

    #[test]
    fn source_picker_keeps_remote_rows_and_collapses_equal_local_paths() {
        let spaces = vec![
            space("remote", "device-old", "/repo", 0),
            space("local-old", "device-current", "/repo/.", 1),
            space("local-new", "device-current", "/repo", 4),
        ];

        let choices = source_picker_spaces(spaces, Some("device-current"));
        let ids: Vec<&str> = choices
            .iter()
            .map(|source| source.space.id.as_str())
            .collect();

        assert_eq!(ids, ["remote", "local-new"]);
    }
    #[test]
    fn labels_detached_codex_worktrees_by_checkout_id() {
        assert_eq!(
            detached_worktree_label("/Users/alex/.codex/worktrees/6666/ashler-platform"),
            Some("6666 (detached)".into())
        );
        assert_eq!(
            detached_worktree_label(
                "/Users/alex/repo/.comet-native/worktrees/ashler-platform/fix-sidebar"
            ),
            Some("fix-sidebar (detached)".into())
        );
    }

    #[test]
    fn replaces_synthetic_device_names_in_folder_status() {
        assert_eq!(folder_device_name(Some("unknown-device")), "Remote device");
        assert_eq!(folder_device_name(Some("Studio Mac")), "Studio Mac");
    }
}
