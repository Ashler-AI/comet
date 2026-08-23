//! The app shell (comet `__root.tsx`): sidebar column + main panel + optional
//! right "Changes" pane, plus the boot splash and the connection gate.
//!
//! Layout is comet's: collapsible drag-resizable sidebar (208–400px, default
//! 256) with a 200ms ease-out width transition; main panel with an h-11 header,
//! content outlet, and a reserved h-6 status strip so later content never
//! shifts; right pane scaffold (360–760px, default 520), hidden by default.
//! Widths/collapsed state persist to `ui-settings.json` (debounced).
//!
//! Resize handles use GPUI drag-and-drop (`on_drag` with an empty ghost view
//! plus `on_drag_move::<Marker>` on the root). Double-clicking a handle resets
//! that pane to its default width.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::Utc;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Empty, Entity, Focusable as _, IntoElement,
    KeyBinding, Keystroke, ListAlignment, ListState, MouseButton, MouseDownEvent, MouseUpEvent,
    Pixels, Point, Render, SharedString, Subscription, Task, Window, WindowControlArea, actions,
    div, list, prelude::*, px,
};

use comet_doc::{SessionCommandPayload, SessionControlAction};
use comet_rpc::{GetAgentRouteAccountResult, methods};
use gpui_tokio::Tokio;

use crate::changes::{Changes, ChangesEvent};
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::loaders;
use crate::motion::{self, AnimationExt as _, COMET_PULSE, MotionSpec, RESIZE, SPLASH_OUT};
use crate::popover::{self};
use crate::rail;
use crate::settings::accounts::{AccountsPage, format_reset, usage_color, usage_level};
use crate::settings::advisor::AdvisorPage;
use crate::settings::appearance::AppearancePage;
use crate::settings::archived::ArchivedPage;
use crate::settings::devices::DevicesPage;
use crate::settings::shortcuts::{ShortcutsEvent, ShortcutsPage};
use crate::settings::{
    Density, KeymapConfig, RIGHT_PANE_DEFAULT, RIGHT_PANE_MAX, RIGHT_PANE_MIN, SAVE_DEBOUNCE_MS,
    SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, TERMINAL_DEFAULT_HEIGHT, UiSettings, platform_combo,
};
use crate::state::{
    ActiveHarnessGoal, AppState, ConnectionStatus, EngineBootConfig, GatePhase, Indicator,
    TranscriptEntriesChange, format_time_ago, latest_active_omp_goal,
};
use crate::terminal::panel::{TerminalPanel, ToggleTerminal, clamp_terminal_height};
use crate::theme::Theme;
use crate::transcript::{self, Transcript};

mod spaces;
mod tabs;

use spaces::{AddSpaceFlow, RenameSpaceDialog};

actions!(
    shell,
    [
        ToggleSidebar,
        ToggleChanges,
        AddSpacePalette,
        ToggleCommandPalette,
        ToggleActivity,
        ToggleFocusMode,
        OpenInvite,
        NewSession,
        CloseSession
    ]
);

// ---------------------------------------------------------------------------
// Traffic-light-aware titlebar layout (feature-inventory §1.1)
// ---------------------------------------------------------------------------

/// Where the top-left window-control cluster starts, in px from the window's
/// left edge (comet window-controls.tsx: `left: fullscreen ? 12 : 88`). The
/// frameless hiddenInset chrome puts the macOS traffic lights at {14,15};
/// fullscreen hides them and the cluster reclaims the inset.
pub fn titlebar_cluster_start(fullscreen: bool) -> f32 {
    if fullscreen { 12.0 } else { 88.0 }
}

/// Width of the spacer ahead of the control cluster for a strip that already
/// carries `container_pad` px of its own left padding. macOS only — on
/// Linux/Windows there are no traffic lights and the cluster hugs the edge.
pub fn titlebar_spacer_width(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    if !is_macos {
        return 0.0;
    }
    (titlebar_cluster_start(fullscreen) - container_pad).max(0.0)
}

/// Width of the persistent top-left button cluster itself (sidebar toggle +
/// back/forward: three 24px buttons, 2px gaps).
pub const CLUSTER_BUTTONS_WIDTH: f32 = 24.0 * 3.0 + 2.0 * 2.0;

/// Where the cluster's first button starts, from the window's left edge.
pub fn cluster_buttons_start(is_macos: bool, fullscreen: bool) -> f32 {
    if is_macos {
        titlebar_cluster_start(fullscreen)
    } else {
        10.0
    }
}

fn should_show_composer(has_spaces: bool, has_selection: bool) -> bool {
    has_spaces || has_selection
}

/// Left clearance a full-bleed header (collapsed sidebar) needs so its content
/// starts past the overlay cluster, given the header's own `container_pad`.
pub fn cluster_clearance(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    (cluster_buttons_start(is_macos, fullscreen) + CLUSTER_BUTTONS_WIDTH + 8.0 - container_pad)
        .max(0.0)
}

/// (Re-)apply the whole app keymap: clears every binding, restores the composer
/// map, then binds the customizable shortcuts from `keymap` (feature-inventory
/// §1.4). Invalid persisted combos fall back to that shortcut's default.
pub fn apply_keymap(cx: &mut App, keymap: &KeymapConfig) {
    fn valid_or_default(combo: &str, fallback: &str) -> String {
        let candidate = platform_combo(combo);
        if Keystroke::parse(&candidate).is_ok() {
            candidate
        } else {
            tracing::warn!(%combo, "unparseable shortcut combo; using default");
            platform_combo(fallback)
        }
    }
    cx.clear_key_bindings();
    crate::composer::init(cx);
    // Fixed app-level shortcuts (⌘Q quit, ⇧⌘W close window, ⌘M minimize,
    // ⌘H hide) back the native menu key equivalents and must survive keymap
    // re-application.
    crate::app_menus::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_sidebar, "mod-s"),
            ToggleSidebar,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_changes, "mod-b"),
            ToggleChanges,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_terminal, "mod-j"),
            ToggleTerminal,
            None,
        ),
        KeyBinding::new(&platform_combo("mod-k"), ToggleCommandPalette, None),
        KeyBinding::new(&platform_combo("mod-shift-a"), ToggleActivity, None),
        KeyBinding::new(&platform_combo("mod-shift-f"), ToggleFocusMode, None),
        KeyBinding::new(&platform_combo("mod-shift-i"), OpenInvite, None),
        KeyBinding::new(&platform_combo("mod-n"), NewSession, None),
        KeyBinding::new(&platform_combo("mod-w"), CloseSession, None),
        KeyBinding::new("escape", crate::composer::MentionEscape, None),
    ]);
}

/// The settings sections (feature-inventory §1.5 routes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Devices,
    Agents,
    Advisor,
    Appearance,
    Shortcuts,
    Archived,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 6] = [
        SettingsSection::Devices,
        SettingsSection::Agents,
        SettingsSection::Advisor,
        SettingsSection::Appearance,
        SettingsSection::Shortcuts,
        SettingsSection::Archived,
    ];

    /// Sidebar + header label (comet settings-sidebar.tsx SECTIONS / __root.tsx
    /// `settingsTitle` — the same strings in both places).
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Devices => "Devices",
            SettingsSection::Agents => "Accounts",
            SettingsSection::Advisor => "Advisor",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Shortcuts => "Shortcuts",
            SettingsSection::Archived => "Settled sessions",
        }
    }
}

/// What the main outlet shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Chat,
    Settings(SettingsSection),
}

/// Per-chat panel open flags (comet parity: `sessionPanels` — the terminal and
/// changes panels open *per session*, in memory only; heights and every other
/// persisted setting stay global). New/unknown chats default to closed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChatPanels {
    pub terminal_open: bool,
    pub changes_open: bool,
}

/// The session-scoped panel map. Keys are chat ids; the new-chat canvas uses
/// the empty key. Not persisted — a fresh app starts with everything closed.
#[derive(Debug, Default)]
pub struct SessionPanels {
    map: std::collections::HashMap<String, ChatPanels>,
}

impl SessionPanels {
    pub fn get(&self, key: &str) -> ChatPanels {
        self.map.get(key).copied().unwrap_or_default()
    }

    /// Flip the terminal flag for `key`; returns the new value.
    pub fn toggle_terminal(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.terminal_open = !entry.terminal_open;
        entry.terminal_open
    }

    /// Flip the changes flag for `key`; returns the new value.
    pub fn toggle_changes(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.changes_open = !entry.changes_open;
        entry.changes_open
    }
}

/// One route-history entry (comet parity: the renderer's TanStack memory
/// history — every route the user visited, browser-style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavEntry {
    /// A chat route; the id of the selected chat ("" = the new-chat canvas).
    Chat(String),
    Settings(SettingsSection),
}

/// Browser-style navigation history for the titlebar back/forward buttons
/// (comet window-controls.tsx semantics): every route change pushes an entry;
/// Back/Forward walk the stack without changing it; pushing while behind the
/// tip truncates the entries ahead (a new branch, exactly like a browser).
#[derive(Debug)]
pub struct NavHistory {
    entries: Vec<NavEntry>,
    index: usize,
}

impl NavHistory {
    pub fn new(initial: NavEntry) -> Self {
        Self {
            entries: vec![initial],
            index: 0,
        }
    }

    pub fn current(&self) -> &NavEntry {
        &self.entries[self.index]
    }

    /// Record a route change. Re-navigating to the current route is a no-op
    /// (selecting the already-selected chat never happened as a navigation);
    /// otherwise any forward branch is truncated and the entry appended.
    pub fn push(&mut self, entry: NavEntry) {
        if *self.current() == entry {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(entry);
        self.index += 1;
    }

    /// Swap the current entry in place without growing the stack — the native
    /// equivalent of a `replace: true` navigation (comet's boot redirect from
    /// `/` into the last-used chat leaves no dead Back target behind).
    pub fn replace(&mut self, entry: NavEntry) {
        self.entries[self.index] = entry;
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    /// Memory history keeps every entry, so "behind the last entry" is exactly
    /// "can go forward" (comet window-controls.tsx).
    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn back(&mut self) -> Option<NavEntry> {
        if !self.can_back() {
            return None;
        }
        self.index -= 1;
        Some(self.current().clone())
    }

    pub fn forward(&mut self) -> Option<NavEntry> {
        if !self.can_forward() {
            return None;
        }
        self.index += 1;
        Some(self.current().clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Sidebar resort glide (feature-inventory §1.6): 260ms
/// `cubic-bezier(0.22,1,0.36,1)` per-row translate, the View Transitions
/// equivalent.
pub const RESORT: MotionSpec = MotionSpec::new(260, motion::EASE_RESORT);

/// FLIP diff for a keyed list: given the previously rendered order and the new
/// order (key + row height), return each surviving key's paint-only start
/// offset `old_y - new_y` (only keys whose position actually moved). `gap` is
/// the flex gap between rows. Pure — drives the sidebar resort glide.
pub fn resort_offsets(
    old: &[(String, f32)],
    new: &[(String, f32)],
    gap: f32,
) -> std::collections::HashMap<String, f32> {
    let mut old_y = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in old {
        old_y.insert(key.as_str(), y);
        y += height + gap;
    }
    let mut offsets = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in new {
        if let Some(prev) = old_y.get(key.as_str()) {
            let dy = prev - y;
            if dy.abs() > 0.5 {
                offsets.insert(key.clone(), dy);
            }
        }
        y += height + gap;
    }
    offsets
}

/// Rich rows keep four compact information bands. Density changes surrounding
/// chrome only, never the type scale or the information shown.
fn chat_row_height(density: Density) -> f32 {
    match density {
        Density::Compact => 57.0,
        Density::Comfortable => 66.0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalSessionCapability {
    Resume,
    ImportHistory,
    Unavailable,
}

impl LocalSessionCapability {
    fn from_flags(resumable: bool, history_only: bool) -> Self {
        match (resumable, history_only) {
            (true, false) => Self::Resume,
            (false, true) => Self::ImportHistory,
            _ => Self::Unavailable,
        }
    }

    fn source_label(self, busy_elsewhere: bool) -> &'static str {
        if busy_elsewhere {
            return "Running";
        }
        match self {
            Self::Resume => "Existing",
            Self::ImportHistory => "History only",
            Self::Unavailable => "Unknown",
        }
    }

    fn action_label(self, busy_elsewhere: bool) -> &'static str {
        if busy_elsewhere && matches!(self, Self::Unavailable) {
            return "In use";
        }
        match self {
            Self::Resume => "Open",
            Self::ImportHistory => "Import",
            Self::Unavailable => "Unavailable",
        }
    }

    fn can_attach(self) -> bool {
        !matches!(self, Self::Unavailable)
    }
}

fn local_session_import_completed(
    target_chat: Option<&str>,
    selected_chat: Option<&str>,
    target_still_available: bool,
) -> bool {
    target_chat.is_some_and(|target| selected_chat == Some(target) && !target_still_available)
}

#[derive(Debug, Clone, PartialEq)]
struct LocalSessionProviderSection {
    harness: comet_proto::HarnessId,
    sessions: Vec<comet_proto::LocalSessionCandidate>,
}

fn local_session_provider_sections(
    candidates: &[comet_proto::LocalSessionCandidate],
) -> Vec<LocalSessionProviderSection> {
    const PROVIDERS: [comet_proto::HarnessId; 7] = [
        comet_proto::HarnessId::Omp,
        comet_proto::HarnessId::ClaudeCode,
        comet_proto::HarnessId::Codex,
        comet_proto::HarnessId::PrimeAgent,
        comet_proto::HarnessId::OpenCode,
        comet_proto::HarnessId::Cursor,
        comet_proto::HarnessId::Mock,
    ];

    PROVIDERS
        .into_iter()
        .filter_map(|harness| {
            let mut sessions: Vec<_> = candidates
                .iter()
                .filter(|candidate| candidate.harness == harness)
                .cloned()
                .collect();
            sessions.sort_by(|a, b| {
                b.updated_at
                    .cmp(&a.updated_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            (!sessions.is_empty()).then_some(LocalSessionProviderSection { harness, sessions })
        })
        .collect()
}

const LOCAL_SESSION_IMPORT_CONTENT_HEIGHT: f32 = 416.0;
const LOCAL_SESSION_IMPORT_DIALOG_WIDTH: f32 = 680.0;
const LOCAL_SESSION_PROVIDER_HEADER_HEIGHT: f32 = 38.0;
const LOCAL_SESSION_PROVIDER_ROW_HEIGHT: f32 = 104.0;
const LOCAL_SESSION_PROVIDER_MAX_HEIGHT: f32 = LOCAL_SESSION_PROVIDER_ROW_HEIGHT * 4.0;
const LOCAL_SESSION_PROVIDER_FOLD_WINDOW: Duration = Duration::from_millis(400);

fn local_session_provider_viewport_height(session_count: usize) -> f32 {
    (session_count as f32 * LOCAL_SESSION_PROVIDER_ROW_HEIGHT)
        .min(LOCAL_SESSION_PROVIDER_MAX_HEIGHT)
}
fn local_session_provider_scroll_distance(
    session_count: usize,
    current: gpui::ListOffset,
    desired_delta: Pixels,
) -> Pixels {
    let current_offset =
        px(current.item_ix as f32 * LOCAL_SESSION_PROVIDER_ROW_HEIGHT) + current.offset_in_item;
    let max_offset = px((session_count as f32 * LOCAL_SESSION_PROVIDER_ROW_HEIGHT
        - local_session_provider_viewport_height(session_count))
    .max(0.0));
    let target_offset = (current_offset + desired_delta)
        .max(px(0.0))
        .min(max_offset);
    target_offset - current_offset
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LocalSessionProviderFold {
    expanded: bool,
    epoch: usize,
    from: f32,
    toggled_at: Option<std::time::Instant>,
}

impl Default for LocalSessionProviderFold {
    fn default() -> Self {
        Self {
            expanded: false,
            epoch: 0,
            from: 0.0,
            toggled_at: None,
        }
    }
}

impl LocalSessionProviderFold {
    fn toggle(&mut self, expanded_height: f32) {
        self.from = if self.expanded { expanded_height } else { 0.0 };
        self.expanded = !self.expanded;
        self.epoch += 1;
        self.toggled_at = Some(std::time::Instant::now());
    }

    fn animating(self) -> bool {
        self.epoch > 0
            && self
                .toggled_at
                .is_some_and(|at| at.elapsed() < LOCAL_SESSION_PROVIDER_FOLD_WINDOW)
    }
}

fn imported_chat_history_source(
    chat_id: &str,
    harness_session_id: Option<&str>,
) -> Option<&'static str> {
    if !chat_id.starts_with("local-chat-") || harness_session_id.is_some() {
        return None;
    }
    Some(if chat_id.starts_with("local-chat-opencode-") {
        "OpenCode"
    } else {
        "Imported"
    })
}

fn local_session_age(updated_at: i64, now: chrono::DateTime<Utc>) -> String {
    chrono::DateTime::<Utc>::from_timestamp_millis(updated_at)
        .map(|updated| format_time_ago(updated, now))
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
struct SidebarSessionMeta {
    source: comet_proto::AgentSessionSource,
    runtime_model: SharedString,
    scaffold_web: Option<SharedString>,
    scaffold_session: Option<SharedString>,
}
/// Flex gap between sidebar list items.
const SIDEBAR_LIST_GAP: f32 = 2.0;

/// Ramp height of the glass sidebar's scroll-edge fade (the gpui
/// [`gpui::EdgeFade`] scope — per-primitive, so text fades per glyph).
const SIDEBAR_GLASS_FADE_BAND: f32 = 32.0;

/// Width reserved beside the conversation for the always-visible floating
/// worktree/account/goals card. The rail itself is transparent; only the card
/// paints, so this behaves like the Changes pane without reading as a panel.
const WORKSPACE_STATUS_RAIL_WIDTH: f32 = 348.0;
const WORKSPACE_GOALS_MAX_HEIGHT: f32 = 232.0;

/// Drag marker for the sidebar resize handle.
struct SidebarResize;
/// Drag marker for the right-pane resize handle.
struct RightPaneResize;
/// Drag marker for the terminal-panel height handle.
struct TerminalResize;

/// Invisible drag ghost — resize drags render nothing at the cursor.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A oneshot width tween (200ms ease-out), driven MANUALLY from render via
/// [`Shell::eval_tween`] — never through a `with_animation` wrapper. gpui keys
/// an animation element's start time by its full global element-id path, so a
/// wrapper that mounts/remounts (route swap, or an ancestor animation keyed by
/// a fresh epoch) silently REPLAYS the tween from t=0. Manual evaluation keeps
/// the element tree's shape constant: a finished or stale tween is exactly the
/// steady state, no matter how the tree around it remounts (round-6 §1–3).
#[derive(Debug, Clone, Copy)]
struct WidthTween {
    from: f32,
    to: f32,
    started: std::time::Instant,
}

impl WidthTween {
    fn new(from: f32, to: f32) -> Self {
        Self {
            from,
            to,
            started: std::time::Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplashPhase {
    Visible,
    FadingOut,
    Gone,
}

fn next_splash_phase(current: SplashPhase, connection: &ConnectionStatus) -> SplashPhase {
    match connection {
        ConnectionStatus::Ready if current == SplashPhase::Visible => SplashPhase::FadingOut,
        // Reveal the gate card immediately; the splash never returns mid-session.
        ConnectionStatus::Failed(_) => SplashPhase::Gone,
        ConnectionStatus::Ready | ConnectionStatus::Connecting => current,
    }
}

/// The chat-row Rename dialog.
struct RenameChatDialog {
    chat_id: String,
    input: Entity<ComposerInput>,
    /// Focus the input on the dialog's first paint (opened without window access).
    focus_pending: bool,
    _events: Subscription,
}

/// Optimistic audit row for a remotely-owned control. The collaboration
/// publication reconciles this to Applied/Rejected without hiding it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ControlFeedbackState {
    Pending,
    Applied,
    Rejected,
}

#[derive(Debug, Clone)]
struct ControlFeedback {
    command_id: String,
    actor: SharedString,
    action: SharedString,
    occurred_at: i64,
    state: ControlFeedbackState,
    detail: Option<SharedString>,
}

/// In-app update lifecycle (macOS bundle installs; see `render_update_strip`).
enum UpdateFlow {
    Idle,
    Downloading,
    /// Staged bundle ready to swap in — one click restarts into it.
    Ready(PathBuf),
    Failed(SharedString),
}

struct AnnotationInspector {
    annotation: comet_proto::SemanticAnnotation,
    /// Captured when the inspector opens; mutations never retarget when the
    /// user selects another session row.
    session_id: Option<String>,
    is_new: bool,
    input: Entity<ComposerInput>,
    error: Option<SharedString>,
    /// Window-space origin for a compact transcript-selection comment pop-up.
    /// Other annotation entry points keep using the full inspector drawer.
    popup_origin: Option<Point<Pixels>>,
    /// Holds the input's event subscription for the inspector's lifetime —
    /// Enter (Submit) saves the comment like the Comment/Save button.
    _input_sub: gpui::Subscription,
}

fn right_drawer_overlay(viewport: gpui::Size<Pixels>, drawer: impl IntoElement) -> AnyElement {
    gpui::deferred(
        gpui::anchored()
            .position(gpui::point(px(0.0), px(0.0)))
            .child(
                div()
                    .occlude()
                    .w(viewport.width)
                    .h(viewport.height)
                    .bg(crate::theme::scrim(0.45))
                    .flex()
                    .justify_end()
                    .child(drawer),
            ),
    )
    .priority(2)
    .into_any_element()
}

fn annotation_prompt_context(annotation: &comet_proto::SemanticAnnotation) -> String {
    match annotation
        .anchor
        .exact
        .as_deref()
        .map(str::trim)
        .filter(|exact| !exact.is_empty())
    {
        Some(exact) => format!(
            "Selected text:\n{exact}\n\nComment:\n{}",
            annotation.body.trim()
        ),
        None => format!("Comment:\n{}", annotation.body.trim()),
    }
}

fn latest_goal_items(
    entries: &[comet_doc::SessionMessageEntry],
) -> Option<&[comet_proto::TodoItem]> {
    entries
        .iter()
        .rev()
        .flat_map(|entry| entry.parts.iter().rev())
        .find_map(|part| match part {
            comet_doc::MessagePart::Tool {
                call: comet_proto::ToolCall::Todo { items },
                ..
            } => Some(items.as_slice()),
            _ => None,
        })
}

#[derive(Debug, Clone, PartialEq)]
struct GoalGroupData {
    label: Option<String>,
    items: Vec<comet_proto::TodoItem>,
}

fn todo_items_from_value(value: Option<&serde_json::Value>) -> Vec<comet_proto::TodoItem> {
    value
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| comet_proto::TodoItem {
            text: text.to_string(),
            done: false,
        })
        .collect()
}

/// Replays OMP's persisted `todo` tool inputs. ACP's `plan` update is flat,
/// while the originating tool input retains phase/list structure. Other
/// harnesses fall back to their latest flat `ToolCall::Todo` snapshot.
fn structured_goal_groups(
    entries: &[comet_doc::SessionMessageEntry],
) -> Option<Vec<GoalGroupData>> {
    let mut groups: Option<Vec<GoalGroupData>> = None;
    for part in entries.iter().flat_map(|entry| &entry.parts) {
        let comet_doc::MessagePart::Tool {
            call:
                comet_proto::ToolCall::Unknown {
                    input: Some(input), ..
                },
            ..
        } = part
        else {
            continue;
        };
        let Some(op) = input.get("op").and_then(serde_json::Value::as_str) else {
            continue;
        };
        match op {
            "init" => {
                if let Some(list) = input.get("list").and_then(serde_json::Value::as_array) {
                    groups = Some(
                        list.iter()
                            .filter_map(|group| {
                                let label = group
                                    .get("phase")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::trim)
                                    .filter(|label| !label.is_empty())?
                                    .to_string();
                                Some(GoalGroupData {
                                    label: Some(label),
                                    items: todo_items_from_value(group.get("items")),
                                })
                            })
                            .collect(),
                    );
                } else if input.get("items").is_some() {
                    groups = Some(vec![GoalGroupData {
                        label: None,
                        items: todo_items_from_value(input.get("items")),
                    }]);
                }
            }
            "append" => {
                let Some(groups) = groups.as_mut() else {
                    continue;
                };
                let Some(label) = input
                    .get("phase")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                else {
                    continue;
                };
                let items = todo_items_from_value(input.get("items"));
                if let Some(group) = groups
                    .iter_mut()
                    .find(|group| group.label.as_deref() == Some(label))
                {
                    group.items.extend(items);
                } else {
                    groups.push(GoalGroupData {
                        label: Some(label.to_string()),
                        items,
                    });
                }
            }
            "done" => {
                let Some(groups) = groups.as_mut() else {
                    continue;
                };
                if let Some(task) = input
                    .get("task")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|task| !task.is_empty())
                {
                    if let Some(item) = groups
                        .iter_mut()
                        .flat_map(|group| &mut group.items)
                        .find(|item| item.text == task)
                    {
                        item.done = true;
                    }
                } else if let Some(phase) = input
                    .get("phase")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|phase| !phase.is_empty())
                    && let Some(group) = groups
                        .iter_mut()
                        .find(|group| group.label.as_deref() == Some(phase))
                {
                    group.items.iter_mut().for_each(|item| item.done = true);
                }
            }
            "drop" | "rm" => {
                let Some(groups) = groups.as_mut() else {
                    continue;
                };
                let task = input
                    .get("task")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|task| !task.is_empty());
                let phase = input
                    .get("phase")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|phase| !phase.is_empty());
                if let Some(task) = task {
                    for group in groups {
                        group.items.retain(|item| item.text != task);
                    }
                } else if let Some(phase) = phase {
                    groups.retain(|group| group.label.as_deref() != Some(phase));
                } else if op == "rm" {
                    groups.clear();
                }
            }
            _ => {}
        }
    }
    groups
}

fn latest_goal_groups(entries: &[comet_doc::SessionMessageEntry]) -> Vec<GoalGroupData> {
    structured_goal_groups(entries).unwrap_or_else(|| {
        latest_goal_items(entries)
            .map(|items| {
                vec![GoalGroupData {
                    label: None,
                    items: items.to_vec(),
                }]
            })
            .unwrap_or_default()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalRowData {
    text: String,
    done: bool,
    depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalGroupRows {
    label: Option<String>,
    rows: Vec<GoalRowData>,
}

fn goal_group_rows(groups: Vec<GoalGroupData>) -> Vec<GoalGroupRows> {
    groups
        .into_iter()
        .map(|group| GoalGroupRows {
            label: group.label,
            rows: goal_rows(&group.items),
        })
        .collect()
}

/// Transcript-derived values painted by the shell rather than by the independently
/// reactive transcript entity. Equality is the transcript lane's invalidation key:
/// text deltas are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptChromeProjection {
    goal_groups: Vec<GoalGroupRows>,
    active_goal: Option<ActiveHarnessGoal>,
    shared_session_previews: Vec<(String, Option<String>)>,
    selected_agent_indicator: Indicator,
}

impl Default for TranscriptChromeProjection {
    fn default() -> Self {
        Self {
            goal_groups: Vec::new(),
            active_goal: None,
            shared_session_previews: Vec::new(),
            selected_agent_indicator: Indicator::None,
        }
    }
}

/// Goal calls are the only transcript payload that can change the shell's goal
/// projection. One compact entry-aligned lane lets exact transcript splices
/// prove that a plain-text delta cannot affect goals without replaying history.
type GoalEntryProjection = Vec<(String, comet_proto::ToolCall)>;

struct TranscriptChromeCache {
    revision: u64,
    goal_entries: Vec<GoalEntryProjection>,
    projection: TranscriptChromeProjection,
}

impl TranscriptChromeCache {
    fn new(state: &AppState, now: chrono::DateTime<Utc>) -> Self {
        Self {
            revision: state.transcript_revision(),
            goal_entries: state.transcript.iter().map(goal_entry_projection).collect(),
            projection: TranscriptChromeProjection {
                goal_groups: goal_group_rows(latest_goal_groups(&state.transcript)),
                active_goal: latest_active_omp_goal(&state.transcript),
                shared_session_previews: shared_session_previews(state),
                selected_agent_indicator: state.selected_agent_indicator(now),
            },
        }
    }

    fn refresh(&mut self, state: &AppState, now: chrono::DateTime<Utc>) -> bool {
        let mut changed = false;
        if self.revision != state.transcript_revision() {
            if self.reconcile_goal_entries(state) {
                let goal_groups = goal_group_rows(latest_goal_groups(&state.transcript));
                let active_goal = latest_active_omp_goal(&state.transcript);
                changed |= self.projection.goal_groups != goal_groups
                    || self.projection.active_goal != active_goal;
                self.projection.goal_groups = goal_groups;
                self.projection.active_goal = active_goal;
            }
            self.revision = state.transcript_revision();
        }
        if !shared_session_previews_match(&self.projection.shared_session_previews, state) {
            self.projection.shared_session_previews = shared_session_previews(state);
            changed = true;
        }
        let selected_agent_indicator = state.selected_agent_indicator(now);
        if self.projection.selected_agent_indicator != selected_agent_indicator {
            self.projection.selected_agent_indicator = selected_agent_indicator;
            changed = true;
        }
        changed
    }

    /// Applies one exact AppState transcript delta to the compact goal lane.
    /// `true` means goal calls changed and the structured replay is required.
    fn reconcile_goal_entries(&mut self, state: &AppState) -> bool {
        let change = state.transcript_change();
        if self.revision.wrapping_add(1) != change.revision {
            self.goal_entries = state.transcript.iter().map(goal_entry_projection).collect();
            return true;
        }
        match &change.entries {
            TranscriptEntriesChange::None => false,
            TranscriptEntriesChange::Reset => {
                self.goal_entries = state.transcript.iter().map(goal_entry_projection).collect();
                true
            }
            TranscriptEntriesChange::Splice { old, new } => {
                if old.end > self.goal_entries.len() || new.end > state.transcript.len() {
                    self.goal_entries =
                        state.transcript.iter().map(goal_entry_projection).collect();
                    return true;
                }
                let new_entries = &state.transcript[new.clone()];
                let unchanged = old.len() == new.len()
                    && self.goal_entries[old.clone()]
                        .iter()
                        .zip(new_entries)
                        .all(|(cached, entry)| goal_entry_projection_matches(cached, entry));
                if unchanged {
                    return false;
                }
                let replacement = new_entries
                    .iter()
                    .map(goal_entry_projection)
                    .collect::<Vec<_>>();
                let goals_changed =
                    goal_entry_ranges_differ(&self.goal_entries[old.clone()], &replacement);
                drop(self.goal_entries.splice(old.clone(), replacement));
                goals_changed
            }
        }
    }
}

fn goal_entry_projection(entry: &comet_doc::SessionMessageEntry) -> GoalEntryProjection {
    entry
        .parts
        .iter()
        .filter_map(|part| {
            let comet_doc::MessagePart::Tool { id, call, .. } = part else {
                return None;
            };
            goal_call_is_relevant(id, call).then(|| (id.clone(), call.clone()))
        })
        .collect()
}

fn goal_entry_projection_matches(
    cached: &GoalEntryProjection,
    entry: &comet_doc::SessionMessageEntry,
) -> bool {
    cached
        .iter()
        .map(|(id, call)| (id.as_str(), call))
        .eq(entry.parts.iter().filter_map(|part| {
            let comet_doc::MessagePart::Tool { id, call, .. } = part else {
                return None;
            };
            goal_call_is_relevant(id, call).then_some((id.as_str(), call))
        }))
}

fn goal_entry_ranges_differ(old: &[GoalEntryProjection], new: &[GoalEntryProjection]) -> bool {
    old.iter().flatten().ne(new.iter().flatten())
}

fn goal_call_is_relevant(id: &str, call: &comet_proto::ToolCall) -> bool {
    match call {
        comet_proto::ToolCall::Todo { .. } => true,
        comet_proto::ToolCall::Unknown { name, input } => {
            (id == comet_proto::OMP_GOAL_STATE_CALL_ID
                || name == comet_proto::OMP_GOAL_STATE_CALL_NAME)
                || input
                    .as_ref()
                    .and_then(|input| input.get("op"))
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|op| matches!(op, "init" | "append" | "done" | "drop" | "rm"))
        }
        _ => false,
    }
}

fn shared_session_previews(state: &AppState) -> Vec<(String, Option<String>)> {
    state
        .shared_session_refs()
        .map(|session_ref| {
            (
                session_ref.chat_id.clone(),
                state
                    .shared_session_preview(&session_ref.chat_id)
                    .map(str::to_owned),
            )
        })
        .collect()
}

#[derive(Clone, PartialEq)]
struct ShellChatProjection {
    id: String,
    status: comet_proto::ChatIndicator,
    scaffold_starting: bool,
    scaffold_environment: Option<comet_proto::SessionEnvironment>,
}

fn shared_session_previews_match(cached: &[(String, Option<String>)], state: &AppState) -> bool {
    cached.len() == state.shared_session_refs().count()
        && cached.iter().zip(state.shared_session_refs()).all(
            |((cached_id, cached_preview), session_ref)| {
                cached_id == &session_ref.chat_id
                    && cached_preview.as_deref()
                        == state.shared_session_preview(&session_ref.chat_id)
            },
        )
}

/// Non-transcript AppState inputs painted or acted on by the shell. The cached
/// snapshot is only cloned after equality fails, so text-token notifications
/// do not rebuild the larger state vectors.
#[derive(Clone, PartialEq)]
struct ShellStateProjection {
    connection: ConnectionStatus,
    auth: Option<comet_proto::AuthState>,
    devices: Vec<comet_proto::Device>,
    spaces: Vec<comet_proto::Space>,
    chats: Vec<comet_proto::Chat>,
    local_session_candidates: Vec<comet_proto::LocalSessionCandidate>,
    local_sessions_loading: bool,
    local_sessions_error: Option<String>,
    local_session_attaching: std::collections::HashSet<String>,
    local_session_attach_errors: std::collections::HashMap<String, String>,
    sessions: Vec<comet_proto::Session>,
    session_refs: Vec<comet_proto::SessionRef>,
    selected_space: Option<String>,
    selected_chat: Option<String>,
    collaboration: Option<comet_proto::CollaborationSnapshot>,
    selected_agent_session: Option<String>,
    selected_invitation_grant: Option<String>,
    local_device_id: Option<String>,
    update: Option<comet_update::UpdateStatus>,
    scaffold_session_creating: bool,
    scaffold_session_error: Option<String>,
    chat_projections: Vec<ShellChatProjection>,
}

impl ShellStateProjection {
    fn capture(state: &AppState, now: chrono::DateTime<Utc>) -> Self {
        Self {
            connection: state.connection.clone(),
            auth: state.auth.clone(),
            devices: state.devices.clone(),
            spaces: state.spaces.clone(),
            chats: state.chats.clone(),
            local_session_candidates: state.local_session_candidates.clone(),
            local_sessions_loading: state.local_sessions_loading,
            local_sessions_error: state.local_sessions_error.clone(),
            local_session_attaching: state.local_session_attaching.clone(),
            local_session_attach_errors: state.local_session_attach_errors.clone(),
            sessions: state.sessions.clone(),
            session_refs: state.session_refs.clone(),
            selected_space: state.selected_space.clone(),
            selected_chat: state.selected_chat.clone(),
            collaboration: state.collaboration.clone(),
            selected_agent_session: state.selected_agent_session.clone(),
            selected_invitation_grant: state.selected_invitation_grant.clone(),
            local_device_id: state.local_device_id.clone(),
            update: state.update.clone(),
            scaffold_session_creating: state.scaffold_session_creating(),
            scaffold_session_error: state.scaffold_session_error.clone(),
            chat_projections: shell_chat_projections(state, now),
        }
    }

    fn matches(&self, state: &AppState, now: chrono::DateTime<Utc>) -> bool {
        self.connection == state.connection
            && self.auth == state.auth
            && self.devices == state.devices
            && self.spaces == state.spaces
            && self.chats == state.chats
            && self.local_session_candidates == state.local_session_candidates
            && self.local_sessions_loading == state.local_sessions_loading
            && self.local_sessions_error == state.local_sessions_error
            && self.local_session_attaching == state.local_session_attaching
            && self.local_session_attach_errors == state.local_session_attach_errors
            && self.sessions == state.sessions
            && self.session_refs == state.session_refs
            && self.selected_space == state.selected_space
            && self.selected_chat == state.selected_chat
            && self.collaboration == state.collaboration
            && self.selected_agent_session == state.selected_agent_session
            && self.selected_invitation_grant == state.selected_invitation_grant
            && self.local_device_id == state.local_device_id
            && self.update == state.update
            && self.scaffold_session_creating == state.scaffold_session_creating()
            && self.scaffold_session_error == state.scaffold_session_error
            && shell_chat_projections_match(&self.chat_projections, state, now)
    }
}

fn shell_chat_projections(
    state: &AppState,
    now: chrono::DateTime<Utc>,
) -> Vec<ShellChatProjection> {
    state
        .chats
        .iter()
        .map(|chat| ShellChatProjection {
            id: chat.id.clone(),
            status: state.display_status_for(chat, now),
            scaffold_starting: state.scaffold_chat_starting(&chat.id),
            scaffold_environment: state.scaffold_environment(&chat.id).cloned(),
        })
        .collect()
}

fn shell_chat_projections_match(
    cached: &[ShellChatProjection],
    state: &AppState,
    now: chrono::DateTime<Utc>,
) -> bool {
    cached.len() == state.chats.len()
        && cached.iter().zip(&state.chats).all(|(cached, chat)| {
            cached.id == chat.id
                && cached.status == state.display_status_for(chat, now)
                && cached.scaffold_starting == state.scaffold_chat_starting(&chat.id)
                && cached.scaffold_environment.as_ref() == state.scaffold_environment(&chat.id)
        })
}

#[cfg(test)]
fn shell_invalidation_changed(
    previous_state: &ShellStateProjection,
    next_state: &ShellStateProjection,
    previous_transcript: &TranscriptChromeProjection,
    next_transcript: &TranscriptChromeProjection,
) -> bool {
    previous_state != next_state || previous_transcript != next_transcript
}

/// Split one goal line into indentation, optional checkbox state, list-marker
/// presence, and display text. This accepts markdown-style bullets, numbered
/// lists, and checkboxes without requiring goal producers to share a schema.
fn strip_goal_list_marker(line: &str) -> (usize, Option<bool>, bool, &str) {
    let trimmed = line.trim_start();
    let indent = line.len().saturating_sub(trimmed.len()) / 2;
    let mut body = trimmed;
    let mut marked = false;

    for marker in ["- ", "* ", "+ ", "• "] {
        if let Some(rest) = body.strip_prefix(marker) {
            body = rest;
            marked = true;
            break;
        }
    }
    if !marked {
        let digits = body.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            let suffix = &body[digits..];
            if let Some(rest) = suffix
                .strip_prefix(". ")
                .or_else(|| suffix.strip_prefix(") "))
            {
                body = rest;
                marked = true;
            }
        }
    }

    let done = if let Some(rest) = body
        .strip_prefix("[x] ")
        .or_else(|| body.strip_prefix("[X] "))
    {
        body = rest;
        marked = true;
        Some(true)
    } else if let Some(rest) = body.strip_prefix("[ ] ") {
        body = rest;
        marked = true;
        Some(false)
    } else {
        None
    };
    (indent, done, marked, body.trim())
}

/// Preserve the complete ordered goal list. A multiline goal may carry a
/// nested markdown/indented sublist; continuation prose stays on its parent.
fn goal_rows(items: &[comet_proto::TodoItem]) -> Vec<GoalRowData> {
    let mut rows = Vec::new();
    for item in items {
        let first_row = rows.len();
        for line in item.text.lines() {
            let (indent, explicit_done, marked, text) = strip_goal_list_marker(line);
            if text.is_empty() {
                continue;
            }
            if rows.len() == first_row {
                rows.push(GoalRowData {
                    text: text.to_string(),
                    done: explicit_done.unwrap_or(item.done),
                    depth: indent,
                });
            } else if marked || indent > 0 {
                rows.push(GoalRowData {
                    text: text.to_string(),
                    done: explicit_done.unwrap_or(item.done),
                    depth: indent + 1,
                });
            } else if let Some(parent) = rows.last_mut() {
                parent.text.push(' ');
                parent.text.push_str(text);
            }
        }
    }
    rows
}

fn render_goal_row(goal: GoalRowData, index: usize, id_prefix: &str, theme: &Theme) -> AnyElement {
    let marker: AnyElement = if goal.done {
        icon(icons::CHECK)
            .size(px(12.0))
            .text_color(theme.success)
            .into_any_element()
    } else {
        div()
            .size(px(12.0))
            .rounded(px(3.0))
            .border_1()
            .border_color(theme.border_strong)
            .into_any_element()
    };
    let depth = goal.depth.min(8);
    div()
        .id(SharedString::from(format!("{id_prefix}-{index}")))
        .ml(px(depth as f32 * 12.0))
        .py(px(5.0))
        .when(depth > 0, |el| {
            el.pl(px(8.0))
                .border_l_1()
                .border_color(theme.border.opacity(0.7))
        })
        .flex()
        .items_start()
        .gap(px(8.0))
        .child(div().mt(px(2.0)).flex_none().child(marker))
        .child(
            div()
                .min_w_0()
                .text_size(px(11.0))
                .line_height(px(15.0))
                .text_color(if goal.done {
                    theme.text_faint
                } else {
                    theme.text
                })
                .child(SharedString::from(goal.text)),
        )
        .into_any_element()
}

fn render_goal_group(
    group: GoalGroupRows,
    group_index: usize,
    id_prefix: &str,
    theme: &Theme,
) -> AnyElement {
    let total = group.rows.len();
    let done = group.rows.iter().filter(|row| row.done).count();
    let row_prefix = format!("{id_prefix}-{group_index}");
    let rows = group
        .rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| render_goal_row(row, index, &row_prefix, theme))
        .collect::<Vec<_>>();
    let labelled = group.label.is_some();
    div()
        .id(SharedString::from(format!(
            "{id_prefix}-group-{group_index}"
        )))
        .when_some(group.label.map(SharedString::from), |el, label| {
            el.pt(px(7.0)).child(
                div()
                    .pb(px(3.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if done == total && total > 0 {
                        theme.text_faint
                    } else {
                        theme.text_muted
                    })
                    .child(label)
                    .child(div().flex_1())
                    .child(SharedString::from(format!("{done}/{total}"))),
            )
        })
        .child(
            div()
                .when(labelled, |el| {
                    el.ml(px(4.0))
                        .pl(px(8.0))
                        .border_l_1()
                        .border_color(theme.border.opacity(0.7))
                })
                .children(rows),
        )
        .into_any_element()
}

/// Press bookkeeping for the session row's settle button: the row is
/// clickable (select) and the button lives inside it, but gpui's
/// `stop_propagation` does NOT suppress an ancestor's click listener from a
/// descendant's — measured on the pinned rev, both fire, descendant first.
/// So the row has to recognize the button's click and stand down.
///
/// The press is the only reliable signal: `on_mouse_down` is hitbox-gated,
/// while hover leave is state-diffed per element path — a row moving between
/// the active and settled lists lands on a fresh path and never sees one.
/// A descendant's press listener runs first, so the button raises
/// [`Self::press_button`] and the row's press claims it; the row presses for
/// EVERY left press it contains, so nothing outlives the click it describes.
#[derive(Default)]
struct SettlePress {
    /// The press in flight landed on a settle button.
    on_button: bool,
    /// The click in flight is the button's, not a row selection.
    owns_click: bool,
}

impl SettlePress {
    fn press_button(&mut self) {
        self.on_button = true;
    }

    fn press_row(&mut self) {
        self.owns_click = std::mem::take(&mut self.on_button);
    }

    /// Whether the click this press produced is the row's to act on.
    fn row_click_selects(&self) -> bool {
        !self.owns_click
    }
}

pub struct Shell {
    state: Entity<AppState>,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    /// Last non-transcript AppState inputs observed by shell chrome.
    state_projection: ShellStateProjection,
    /// Transcript projections painted outside the independently reactive Transcript.
    transcript_chrome: TranscriptChromeCache,
    /// External file drag hovering the conversation column — shows the
    /// "Drop files to attach" veil over the whole chat area; a drop stages
    /// the files in the composer.
    changes_sub: Option<Subscription>,
    file_drag_active: bool,
    /// Terminal stays lazy; Changes starts once the selected-chat status surface mounts.
    terminal: Option<Entity<TerminalPanel>>,
    changes: Option<Entity<Changes>>,
    changes_observation: Option<Subscription>,
    /// Chat outlet vs settings pages.
    route: Route,
    /// Route history behind the titlebar back/forward buttons (§ nav history).
    nav: NavHistory,
    devices_page: Option<Entity<DevicesPage>>,
    archived_page: Option<Entity<ArchivedPage>>,
    advisor_page: Option<Entity<AdvisorPage>>,
    appearance_page: Option<Entity<AppearancePage>>,
    shortcuts_page: Option<Entity<ShortcutsPage>>,
    accounts_page: Option<Entity<AccountsPage>>,
    shortcuts_sub: Option<Subscription>,
    /// Session-row context menu: (chat id, window position).
    chat_menu: Option<(String, Point<Pixels>)>,
    /// Press bookkeeping for the session row's settle button.
    settle_press: SettlePress,
    rename_dialog: Option<RenameChatDialog>,
    /// Chat id awaiting delete confirmation.
    delete_confirm: Option<String>,
    /// Space-row context menu: (space id, window position).
    space_menu: Option<(String, Point<Pixels>)>,
    rename_space_dialog: Option<RenameSpaceDialog>,
    /// Space id awaiting delete confirmation (hard delete + session cascade).
    delete_space_confirm: Option<String>,
    /// The add-space palette (⌘K-style; device tabs + folder search), `Some`
    /// while open.
    add_space: Option<AddSpaceFlow>,
    /// Last selected chat per space (in-memory, like [`SessionPanels`]) — a
    /// space switch lands back on the tab you left.
    space_last_chat: std::collections::HashMap<String, String>,
    /// Session tab currently hovered (close button appears on hover).
    tab_hover: Option<String>,
    /// Session-tab drag-reorder in flight (see `tabs::TabDragState`).
    tab_drag: Option<tabs::TabDragState>,
    /// Space-row drag-reorder in flight (see `spaces::SpaceDragState`).
    space_drag: Option<spaces::SpaceDragState>,
    /// Scroll position of the session tab region (drives the edge fades and
    /// the drop-index math under horizontal overflow).
    tabs_scroll: gpui::ScrollHandle,
    /// Chat id last auto-scrolled into view — scroll-to-selected fires once per
    /// selection change, not every frame (which would fight manual scrolling).
    tabs_scrolled_to: Option<String>,
    /// Scroll position of the sidebar lists region (drives its edge fades).
    sidebar_scroll: gpui::ScrollHandle,
    /// `settings.last_space_id` applied once after the first spaces frame.
    space_boot_applied: bool,
    /// `settings.last_room_id` applied once after the first chat frame.
    room_boot_applied: bool,
    /// Last seen session status per chat — the chime trigger compares against
    /// it (a row's FIRST appearance never chimes, so boot stays silent).
    sound_prev: std::collections::HashMap<String, comet_proto::SessionStatus>,
    user_menu_open: bool,
    /// Session-scoped multiplayer surfaces.
    command_palette_open: bool,
    activity_open: bool,
    invite_open: bool,
    /// Scroll state for the persistent top-right goal list.
    workspace_goals_scroll: gpui::ScrollHandle,
    account_usage: Option<comet_proto::AgentAccountsSnapshot>,
    account_usage_error: Option<SharedString>,
    account_usage_loading: bool,
    account_usage_loaded_at: Option<Instant>,
    account_usage_task: Option<Task<()>>,
    active_account_chat_id: Option<String>,
    active_account_id: Option<String>,
    active_account_loading: bool,
    active_account_loaded_at: Option<Instant>,
    active_account_task: Option<Task<()>>,
    copied_worktree: Option<String>,
    copied_worktree_task: Option<Task<()>>,
    /// On-demand native session picker. Discovery starts only when this opens.
    session_import_open: bool,
    /// Candidate chat selected from the picker; success closes the picker once
    /// AppState selects the materialized chat, while failures remain visible.
    session_import_target_chat: Option<String>,
    session_import_sections: Vec<LocalSessionProviderSection>,
    session_import_lists: std::collections::HashMap<comet_proto::HarnessId, ListState>,
    session_import_folds:
        std::collections::HashMap<comet_proto::HarnessId, LocalSessionProviderFold>,
    session_import_groups_scroll: gpui::ScrollHandle,
    /// Brief copy/open feedback inside the invite surface.
    link_feedback: Option<SharedString>,
    control_feedback: Vec<ControlFeedback>,
    annotation_inspector: Option<AnnotationInspector>,
    /// Set by comment controls; consumed on the next render after the drawer
    /// mounts so typing lands in the note input rather than the composer.
    annotation_focus_pending: bool,
    /// Set when the note input's Submit action fires (Enter); consumed on the
    /// next render, where the window needed by `save_annotation` is in hand.
    annotation_submit_pending: bool,
    control_tasks: Vec<Task<()>>,
    /// Outside-click dismissal instant — suppresses the trigger click that
    /// follows the same mouse-down from instantly reopening the menu.
    user_menu_dismissed_at: Option<std::time::Instant>,
    /// Inline sidebar error strip (mutation failures); click dismisses.
    sidebar_notice: Option<SharedString>,
    /// In-flight add/remove session membership RPC.
    session_ref_task: Option<Task<()>>,
    /// Local lifecycle of an in-app update (macOS bundle swap) — the engine's
    /// UpdateStatus stream says WHETHER one exists; this says how far the
    /// download/stage of it has come in this process.
    update_flow: UpdateFlow,
    update_task: Option<Task<()>>,
    /// Version whose update strip the user dismissed (advisory installs only —
    /// a newer release shows the strip again).
    update_dismissed: Option<String>,
    /// How this binary was installed — decides the strip's click behavior.
    /// Cached: `detect_install` stats `current_exe` and this renders per frame.
    install: comet_update::InstallKind,
    mutate_task: Option<Task<()>>,
    auth_task: Option<Task<()>>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    data_dir: PathBuf,
    settings: UiSettings,
    /// Session-scoped panel open flags (terminal / changes per chat; §1.10-1.11
    /// parity — heights stay in [`UiSettings`]).
    panels: SessionPanels,
    /// The panel key of the chat currently shown ("" = new-chat canvas).
    active_chat: String,
    /// Last rendered sidebar order (key + estimated height) — the FLIP baseline
    /// for the §1.6 resort glide.
    sidebar_prev_order: Vec<(String, f32)>,
    /// Per-key paint offsets of the resort in flight, keyed elements restart on
    /// `resort_epoch` bumps.
    sidebar_resort: std::collections::HashMap<String, f32>,
    /// Keys that just appeared in a live list (fade in, no glide).
    sidebar_new_keys: std::collections::HashSet<String>,
    resort_epoch: usize,
    /// Last observed `window.is_window_active()` — rising edge fires a
    /// ProbeSync so a broadcast-deaf room heals as the user looks at the app.
    was_window_active: bool,
    /// Dev/testing knobs (`COMET_OPEN_DIALOG`, `COMET_FORCE_GATE`) — see
    /// [`Shell::new`].
    debug_dialog: Option<String>,
    debug_gate: Option<GatePhase>,
    sidebar_tween: Option<WidthTween>,
    right_tween: Option<WidthTween>,
    terminal_tween: Option<WidthTween>,
    /// Last observed `window.is_fullscreen()` (`None` before first paint) —
    /// flips key the traffic-light inset tween.
    fullscreen: Option<bool>,
    /// 200ms ease-out tween of the cluster start on fullscreen toggles.
    titlebar_tween: Option<WidthTween>,
    /// Armed by mouse-down on a titlebar strip; the next mouse-move hands the
    /// drag to the compositor.
    titlebar_should_move: bool,
    /// Clears the height tween once it completes (so a closed panel unmounts).
    terminal_tween_task: Option<Task<()>>,
    /// Height-drag anchor: (pointer y, height) at mouse-down on the handle.
    terminal_drag_anchor: Option<(f32, f32)>,
    /// `motion::reduced_motion` snapshot, refreshed at the top of each render
    /// pass so [`Shell::eval_tween`] (called from `&self` render helpers) can
    /// snap without a `cx`.
    reduced_motion: bool,
    /// Set by [`Shell::eval_tween`] when any tween is mid-flight this frame;
    /// render schedules the next animation frame off it.
    motion_active: std::cell::Cell<bool>,
    splash: SplashPhase,
    splash_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
    /// Focus fallback (registered on first paint — [`Shell::new`] has no
    /// window): keyboard shortcuts dispatch through the window focus chain, so
    /// with nothing focused they go dead. Initial focus lands on the composer
    /// and focus lost with no successor routes back there.
    focus_sub: Option<Subscription>,
    /// 1s heartbeat re-rendering the working indicator (elapsed + flavour word).
    _ticker: Task<()>,
    _state_observation: Subscription,
    _composer_events: Subscription,
}

impl Shell {
    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let now = Utc::now();
        let state_projection = ShellStateProjection::capture(state.read(cx), now);
        let transcript_chrome = TranscriptChromeCache::new(state.read(cx), now);
        let sound_prev = state
            .read(cx)
            .sessions
            .iter()
            .map(|session| (session.chat_id.clone(), session.status))
            .collect();
        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_app_state_notification(&state, cx);
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        // Own-send re-engages the stick-to-bottom pin with a smooth scroll.
        let composer_events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            move |_this: &mut Shell, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Sent { .. } => {
                    transcript.update(cx, |t, cx| t.on_own_send(cx));
                }
            }
        });
        // Working-indicator heartbeat: notify once a second while a session is
        // live so elapsed time and the flavour word stay fresh.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |shell: &mut Shell, cx| {
                    let live = {
                        let s = shell.state.read(cx);
                        s.selected_chat
                            .as_deref()
                            .is_some_and(|id| s.indicator_for(id, Utc::now()) != Indicator::None)
                    };
                    if live {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let data_dir = boot.data_dir.clone();
        let settings = UiSettings::load(&data_dir);
        // Bind the customizable shortcuts from the persisted keymap.
        apply_keymap(cx, &settings.keymap);
        // Dev/testing knob: `COMET_OPEN_ROUTE=settings[/<section>]` boots
        // straight into a settings section — these pages have no deep link and
        // synthetic input can't reach them on headless compositors.
        let route = match std::env::var("COMET_OPEN_ROUTE").ok().as_deref() {
            Some("settings") | Some("settings/devices") => {
                Route::Settings(SettingsSection::Devices)
            }
            Some("settings/agents") => Route::Settings(SettingsSection::Agents),
            Some("settings/appearance") => Route::Settings(SettingsSection::Appearance),
            Some("settings/advisor") => Route::Settings(SettingsSection::Advisor),
            Some("settings/shortcuts") => Route::Settings(SettingsSection::Shortcuts),
            Some("settings/archived") => Route::Settings(SettingsSection::Archived),
            // `new` pins the new-chat canvas (suppresses boot auto-select).
            Some("new") => {
                state.update(cx, |s, _| s.auto_selected = true);
                Route::Chat
            }
            _ => Route::Chat,
        };
        // Capture knobs for deterministic styling passes.
        let debug_dialog = std::env::var("COMET_OPEN_DIALOG").ok();
        let debug_gate = match std::env::var("COMET_FORCE_GATE").ok().as_deref() {
            Some("signin") => Some(GatePhase::SignIn),
            Some("failed") => Some(GatePhase::Failed("Could not reach the Crew engine".into())),
            _ => None,
        };
        let nav = NavHistory::new(match route {
            Route::Chat => NavEntry::Chat(String::new()),
            Route::Settings(section) => NavEntry::Settings(section),
        });
        let mut shell = Self {
            state,
            transcript,
            composer,
            state_projection,
            transcript_chrome,
            file_drag_active: false,
            terminal: None,
            changes: None,
            route,
            nav,
            changes_sub: None,
            changes_observation: None,
            devices_page: None,
            archived_page: None,
            appearance_page: None,
            shortcuts_page: None,
            accounts_page: None,
            advisor_page: None,
            shortcuts_sub: None,
            chat_menu: None,
            settle_press: SettlePress::default(),
            rename_dialog: None,
            delete_confirm: None,
            space_menu: None,
            rename_space_dialog: None,
            delete_space_confirm: None,
            add_space: None,
            space_last_chat: std::collections::HashMap::new(),
            tab_hover: None,
            tab_drag: None,
            space_drag: None,
            tabs_scroll: gpui::ScrollHandle::new(),
            tabs_scrolled_to: None,
            sidebar_scroll: gpui::ScrollHandle::new(),
            space_boot_applied: false,
            room_boot_applied: false,
            sound_prev,
            user_menu_open: false,
            command_palette_open: false,
            activity_open: false,
            invite_open: false,
            workspace_goals_scroll: gpui::ScrollHandle::new(),
            account_usage: None,
            account_usage_error: None,
            account_usage_loading: false,
            account_usage_loaded_at: None,
            account_usage_task: None,
            active_account_chat_id: None,
            active_account_id: None,
            active_account_loading: false,
            active_account_loaded_at: None,
            active_account_task: None,
            copied_worktree: None,
            copied_worktree_task: None,
            session_import_open: false,
            session_import_target_chat: None,
            session_import_sections: Vec::new(),
            session_import_lists: std::collections::HashMap::new(),
            session_import_folds: std::collections::HashMap::new(),
            session_import_groups_scroll: gpui::ScrollHandle::new(),
            link_feedback: None,
            control_feedback: Vec::new(),
            annotation_inspector: None,
            annotation_focus_pending: false,
            annotation_submit_pending: false,
            control_tasks: Vec::new(),
            user_menu_dismissed_at: None,
            sidebar_notice: None,
            session_ref_task: None,
            update_flow: UpdateFlow::Idle,
            update_task: None,
            update_dismissed: None,
            install: comet_update::detect_install(),
            mutate_task: None,
            auth_task: None,
            boot,
            data_dir,
            settings,
            panels: SessionPanels::default(),
            active_chat: String::new(),
            sidebar_prev_order: Vec::new(),
            sidebar_resort: std::collections::HashMap::new(),
            sidebar_new_keys: std::collections::HashSet::new(),
            resort_epoch: 0,
            was_window_active: false,
            debug_dialog,
            debug_gate,
            sidebar_tween: None,
            right_tween: None,
            terminal_tween: None,
            fullscreen: None,
            titlebar_tween: None,
            titlebar_should_move: false,
            terminal_tween_task: None,
            terminal_drag_anchor: None,
            reduced_motion: false,
            motion_active: std::cell::Cell::new(false),
            splash: SplashPhase::Visible,
            splash_task: None,
            save_task: None,
            focus_sub: None,
            _ticker: ticker,
            _state_observation: observation,
            _composer_events: composer_events,
        };
        let initial_connection = shell.state.read(cx).connection.clone();
        shell.sync_splash(&initial_connection, cx);
        shell.refresh_account_usage(cx);
        shell.refresh_active_agent_account(cx);
        shell
    }

    // ---- splash ----

    fn reconcile_control_feedback(
        feedback_rows: &mut [ControlFeedback],
        audits: &[comet_proto::AuditEvent],
    ) {
        for feedback in feedback_rows {
            if feedback.state != ControlFeedbackState::Pending {
                continue;
            }
            let Some(audit) = crate::multiplayer::command_audit(&feedback.command_id, audits)
            else {
                continue;
            };
            feedback.state = match audit.result {
                comet_proto::AuditResult::Applied => ControlFeedbackState::Applied,
                _ => ControlFeedbackState::Rejected,
            };
            feedback.detail = audit.reason.clone().map(SharedString::from);
        }
    }

    fn sync_splash(&mut self, connection: &ConnectionStatus, cx: &mut Context<Self>) {
        let next = next_splash_phase(self.splash, connection);
        if next == self.splash {
            return;
        }
        self.splash = next;
        if next == SplashPhase::FadingOut {
            self.splash_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(SPLASH_OUT.total() + Duration::from_millis(30))
                    .await;
                this.update(cx, |shell, cx| {
                    shell.splash = SplashPhase::Gone;
                    cx.notify();
                })
                .ok();
            }));
        }
    }

    fn on_app_state_notification(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        let now = Utc::now();
        let state_changed = {
            let current = state.read(cx);
            !self.state_projection.matches(current, now)
        };
        if state_changed {
            self.state_projection = ShellStateProjection::capture(state.read(cx), now);
            // Chimes, optimistic audit reconciliation, navigation/panel switching,
            // and splash transitions only depend on non-transcript state.
            self.on_state_changed(state, cx);
        }
        let transcript_changed = self.transcript_chrome.refresh(state.read(cx), now);
        if transcript_changed {
            self.refresh_active_agent_account(cx);
        }
        if state_changed || transcript_changed {
            cx.notify();
        }
    }

    fn on_state_changed(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        if self.account_usage_loaded_at.is_none() && !self.account_usage_loading {
            self.refresh_account_usage(cx);
        }
        self.refresh_active_agent_account(cx);
        if self.session_import_open {
            self.sync_session_import_sections(cx);
        }
        if self.debug_dialog.as_deref() == Some("local-sessions") {
            self.debug_dialog = None;
            self.open_session_import(cx);
        }
        // Capture knob: the add-space palette needs only the device registry.
        if self.debug_dialog.as_deref() == Some("add-space") && !state.read(cx).devices.is_empty() {
            self.debug_dialog = None;
            self.open_add_space(cx);
        }
        // Capture knob: pop the requested dialog once chats have landed.
        if let Some(which) = self.debug_dialog.clone()
            && let Some(first) = state.read(cx).chats.first().map(|c| c.id.clone())
        {
            self.debug_dialog = None;
            match which.as_str() {
                "rename" => self.open_rename_chat(first, cx),
                "delete" => {
                    self.delete_confirm = Some(first);
                }
                _ => {}
            }
        }
        // Session chimes follow factual session-row updates, never the
        // time-derived display indicator. `effective_indicator` intentionally
        // turns a 45s-old Working row into `None`; treating that visual expiry
        // as Idle produced a phantom Working→Idle completion chime even when no
        // session had updated. Fresh raw transitions still ring for ANY session
        // on any device. A row's first appearance only seeds the baseline, and
        // delayed/backfilled transitions older than the freshness window stay
        // silent.
        {
            let now = Utc::now();
            let app_state = state.read(cx);
            for session in &app_state.sessions {
                let prev = match self.sound_prev.get_mut(session.chat_id.as_str()) {
                    Some(prev) => {
                        let old = *prev;
                        *prev = session.status;
                        old
                    }
                    None => {
                        self.sound_prev
                            .insert(session.chat_id.clone(), session.status);
                        continue;
                    }
                };
                if self.settings.sound_enabled
                    && let Some(sound) = crate::sound::sound_for_session_update(prev, session, now)
                {
                    crate::sound::play(sound);
                }
            }
        }
        // Reconcile optimistic controls with immutable audit publications.
        // Feedback remains visible after resolution so actor and result are
        // never hidden behind a transient toast.
        let audits: Vec<comet_proto::AuditEvent> = state
            .read(cx)
            .collaboration
            .iter()
            .flat_map(|snapshot| snapshot.publications.iter())
            .filter_map(|publication| match &publication.value {
                comet_proto::PublicationValue::Audit(audit) => Some(audit.clone()),
                _ => None,
            })
            .collect();
        Self::reconcile_control_feedback(&mut self.control_feedback, &audits);
        // Restore the last selected space once the first spaces frame lands.
        if !self.space_boot_applied && !state.read(cx).spaces.is_empty() {
            self.space_boot_applied = true;
            if state.read(cx).selected_chat.is_none()
                && let Some(last) = self.settings.last_space_id.clone()
                && state.read(cx).space_row(&last).is_some()
            {
                state.update(cx, |s, cx| s.select_space(Some(last), cx));
            }
        }
        // Restore the last shared thread after the first chat frame. A missing
        // room is ignored; the normal auto-selection remains visible.
        if !self.room_boot_applied && !state.read(cx).chats.is_empty() {
            self.room_boot_applied = true;
            if let Some(last) = self.settings.last_room_id.clone()
                && state.read(cx).chats.iter().any(|chat| chat.id == last)
                && state.read(cx).selected_chat.as_deref() != Some(last.as_str())
            {
                state.update(cx, |s, cx| s.select_chat(Some(last), cx));
            }
        }
        // Track the per-space last chat and persist navigation preferences.
        {
            let (selected_space, selected_chat, chat_space) = {
                let s = state.read(cx);
                let chat_space = s.selected_chat_row().and_then(|c| c.space_id.clone());
                (
                    s.selected_space.clone(),
                    s.selected_chat.clone(),
                    chat_space,
                )
            };
            if selected_chat != self.settings.last_room_id {
                self.settings.last_room_id = selected_chat.clone();
                self.schedule_save(cx);
            }
            if let (Some(space), Some(chat)) = (chat_space, selected_chat) {
                self.space_last_chat.insert(space, chat);
            }
            if selected_space != self.settings.last_space_id && selected_space.is_some() {
                self.settings.last_space_id = selected_space;
                self.schedule_save(cx);
            }
        }
        // Chat switch: restore THAT chat's panel state (per-session open flags;
        // snap, no tween — the panels belong to the destination chat).
        let selected = state.read(cx).selected_chat.clone().unwrap_or_default();
        if selected != self.active_chat {
            self.active_chat = selected;
            // Route history: a chat switch is a navigation. The very first
            // selection off the untouched boot canvas REPLACES that entry —
            // comet's `/` route redirected into the last-used chat, leaving no
            // dead Back target. Walking history lands here too, but the
            // destination already equals `current()`, so the push dedups.
            if matches!(self.route, Route::Chat) {
                let entry = NavEntry::Chat(self.active_chat.clone());
                if self.nav.len() == 1 && *self.nav.current() == NavEntry::Chat(String::new()) {
                    self.nav.replace(entry);
                } else {
                    self.nav.push(entry);
                }
            }
            self.right_tween = None;
            self.terminal_tween = None;
            let panels = self.panels.get(&self.panel_key(cx));
            if let Some(panel) = self.terminal.clone() {
                panel.update(cx, |panel, cx| panel.set_open(panels.terminal_open, cx));
            }
            if panels.changes_open {
                let changes = self.changes_pane(cx);
                changes.update(cx, |changes, cx| changes.ensure_watch(cx));
            }
        }
        let connection = state.read(cx).connection.clone();
        self.sync_splash(&connection, cx);
    }

    // ---- layout state ----

    fn sidebar_target(&self) -> f32 {
        if self.settings.sidebar_collapsed || self.settings.focus_mode {
            0.0
        } else {
            self.settings.sidebar_width
        }
    }
    /// Does the selected space's folder have git? Owner-stamped and synced —
    /// gates the Changes pane, its toggle, and Cmd-B with zero RPCs.
    fn space_git_detected(&self, cx: &App) -> bool {
        self.state.read(cx).selected_space_git()
    }

    /// The current chat's changes-pane flag (per-session, in-memory), gated on
    /// the space having git at all: a stale per-chat open flag must not reopen
    /// the pane after switching into a non-git space.
    /// The per-session panel key. The new-chat canvas (no selection) keys per
    /// SPACE — one shared "" key made a canvas toggle read as global state
    /// (user report).
    fn panel_key(&self, cx: &App) -> String {
        if self.active_chat.is_empty() {
            let space = self
                .state
                .read(cx)
                .selected_space
                .clone()
                .unwrap_or_default();
            format!("space-canvas:{space}")
        } else {
            self.active_chat.clone()
        }
    }

    fn right_pane_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).changes_open && self.space_git_detected(cx)
    }

    /// The current chat's terminal flag (per-session, in-memory).
    fn terminal_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).terminal_open
    }

    fn right_target(&self, cx: &App) -> f32 {
        if self.settings.focus_mode {
            0.0
        } else if self.right_pane_open(cx) {
            self.settings.right_pane_width
        } else {
            0.0
        }
    }
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
        self.schedule_save(cx);
        cx.notify();
    }

    fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        // No git in this space → no diff pane, Cmd-B goes dead.
        if !self.space_git_detected(cx) {
            return;
        }
        let from = self.right_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_changes(&key);
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        if open {
            // The status surface normally starts this watch; opening the pane
            // remains a safe fallback for routes that bypass the toolbar.
            let changes = self.changes_pane(cx);
            changes.update(cx, |changes, cx| changes.ensure_watch(cx));
        }
        cx.notify();
    }

    fn changes_pane(&mut self, cx: &mut Context<Self>) -> Entity<Changes> {
        if let Some(changes) = &self.changes {
            return changes.clone();
        }
        let changes = cx.new(|cx| Changes::new(self.state.clone(), cx));
        let subscription = cx.subscribe(
            &changes,
            |this: &mut Shell, _, event: &ChangesEvent, cx| match event {
                ChangesEvent::OpenAnnotations(anchor) => {
                    this.open_annotation_anchor(anchor.clone(), cx)
                }
            },
        );
        self.changes_sub = Some(subscription);
        let observation = cx.observe(&changes, |_: &mut Shell, _, cx| cx.notify());
        self.changes_observation = Some(observation);
        self.changes = Some(changes.clone());
        changes
    }

    fn refresh_account_usage(&mut self, cx: &mut Context<Self>) {
        if self.account_usage_loading {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.account_usage_loading = true;
        self.account_usage_error = None;
        self.account_usage_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_AGENT_ACCOUNTS, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| {
                shell.account_usage_loading = false;
                shell.account_usage_loaded_at = Some(Instant::now());
                match result {
                    Ok(value) => {
                        match serde_json::from_value::<comet_proto::AgentAccountsSnapshot>(value) {
                            Ok(snapshot) => {
                                shell.account_usage = Some(snapshot);
                                shell.account_usage_error = None;
                            }
                            Err(error) => {
                                shell.account_usage_error =
                                    Some(format!("Account usage unavailable: {error}").into());
                            }
                        }
                    }
                    Err(error) => {
                        shell.account_usage_error =
                            Some(format!("Account usage unavailable: {error}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn refresh_active_agent_account(&mut self, cx: &mut Context<Self>) {
        let (chat_id, target_device_id, configured_account_id) = {
            let state = self.state.read(cx);
            let Some(chat) = state.selected_chat_row() else {
                self.active_account_chat_id = None;
                self.active_account_id = None;
                return;
            };
            (
                chat.id.clone(),
                Some(chat.device_id.clone()),
                chat.config
                    .as_ref()
                    .and_then(|config| config.agent_account_id.clone()),
            )
        };
        if self.active_account_chat_id.as_deref() != Some(chat_id.as_str()) {
            self.active_account_task = None;
            self.active_account_loading = false;
            self.active_account_loaded_at = None;
            self.active_account_chat_id = Some(chat_id.clone());
            self.active_account_id = configured_account_id;
        }
        if self.active_account_loading
            || self
                .active_account_loaded_at
                .is_some_and(|loaded_at| loaded_at.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.active_account_loading = true;
        self.active_account_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::GET_AGENT_ROUTE_ACCOUNT,
                    serde_json::json!({
                        "logicalSessionId": chat_id,
                        "targetDeviceId": target_device_id,
                    }),
                )
                .await;
            this.update(cx, |shell, cx| {
                shell.active_account_loading = false;
                shell.active_account_loaded_at = Some(Instant::now());
                if shell.active_account_chat_id.as_deref() != Some(chat_id.as_str()) {
                    return;
                }
                if let Ok(value) = result
                    && let Ok(account) = serde_json::from_value::<GetAgentRouteAccountResult>(value)
                    && account.account_id.is_some()
                {
                    shell.active_account_id = account.account_id;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn copy_current_worktree(&mut self, cx: &mut Context<Self>) {
        let path = {
            let state = self.state.read(cx);
            state
                .selected_chat_row()
                .and_then(|chat| chat.cwd.clone())
                .or_else(|| state.selected_space_row().map(|space| space.path.clone()))
        };
        let Some(path) = path else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
        self.copied_worktree = Some(path);
        self.copied_worktree_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            this.update(cx, |shell, cx| {
                shell.copied_worktree = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn open_workspace_changes(&mut self, cx: &mut Context<Self>) {
        if self.settings.focus_mode {
            self.toggle_focus_mode(cx);
        }
        if !self.right_pane_open(cx) {
            self.toggle_right_pane(cx);
        }
        cx.notify();
    }

    fn drop_active_omp_goal(&mut self, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.submit_command("/goal drop", cx));
    }

    fn toggle_focus_mode(&mut self, cx: &mut Context<Self>) {
        let sidebar_from = self.sidebar_target();
        let right_from = self.right_target(cx);
        self.settings.focus_mode = !self.settings.focus_mode;
        self.sidebar_tween = Some(WidthTween::new(sidebar_from, self.sidebar_target()));
        self.right_tween = Some(WidthTween::new(right_from, self.right_target(cx)));
        self.schedule_save(cx);
        cx.notify();
    }

    fn toggle_activity(&mut self, cx: &mut Context<Self>) {
        self.activity_open = !self.activity_open;
        self.invite_open = false;
        self.command_palette_open = false;
        self.session_import_open = false;
        cx.notify();
    }

    fn open_invite(&mut self, cx: &mut Context<Self>) {
        self.invite_open = true;
        self.activity_open = false;
        self.command_palette_open = false;
        self.session_import_open = false;
        self.link_feedback = None;
        cx.notify();
    }

    /// Open the invite dialog for one particular session row. Selecting the
    /// chat first keeps the dialog on its single source of truth (the
    /// selected chat) — the same landing an accepted deep link produces.
    fn open_invite_for(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        self.state
            .update(cx, |state, cx| state.select_chat(Some(chat_id), cx));
        self.open_invite(cx);
    }

    fn toggle_command_palette(&mut self, cx: &mut Context<Self>) {
        self.command_palette_open = !self.command_palette_open;

        if self.command_palette_open {
            self.activity_open = false;
            self.invite_open = false;
            self.session_import_open = false;
        }
        cx.notify();
    }
    fn sync_session_import_sections(&mut self, cx: &mut Context<Self>) {
        let candidates = self.state.read(cx).local_session_candidates.clone();
        let sections = local_session_provider_sections(&candidates);
        if sections == self.session_import_sections {
            return;
        }
        if self.session_import_sections.is_empty() && !sections.is_empty() {
            self.session_import_groups_scroll
                .set_offset(Point::default());
        }

        let mut lists = std::collections::HashMap::with_capacity(sections.len());
        for section in &sections {
            let list = self
                .session_import_lists
                .get(&section.harness)
                .cloned()
                .unwrap_or_else(|| {
                    ListState::new(0, ListAlignment::Top, px(LOCAL_SESSION_PROVIDER_MAX_HEIGHT))
                });
            list.reset_with_uniform_height(
                section.sessions.len(),
                px(LOCAL_SESSION_PROVIDER_ROW_HEIGHT),
            );
            lists.insert(section.harness, list);
        }
        self.session_import_folds
            .retain(|harness, _| sections.iter().any(|section| section.harness == *harness));
        self.session_import_lists = lists;
        self.session_import_sections = sections;
    }

    fn toggle_session_import_provider(&mut self, harness: comet_proto::HarnessId) {
        let Some(section) = self
            .session_import_sections
            .iter()
            .find(|section| section.harness == harness)
        else {
            return;
        };
        self.session_import_folds
            .entry(harness)
            .or_default()
            .toggle(local_session_provider_viewport_height(
                section.sessions.len(),
            ));
    }

    fn open_session_import(&mut self, cx: &mut Context<Self>) {
        self.session_import_open = true;
        self.session_import_target_chat = None;
        self.activity_open = false;
        self.invite_open = false;
        self.command_palette_open = false;
        self.session_import_groups_scroll
            .set_offset(Point::default());
        self.sync_session_import_sections(cx);
        self.state.update(cx, |state, cx| {
            state.load_local_sessions(false, cx);
        });
        cx.notify();
    }

    fn handle_escape(&mut self, cx: &mut Context<Self>) {
        if self.command_palette_open {
            self.command_palette_open = false;
        } else if self.activity_open {
            self.activity_open = false;
        } else if self.invite_open {
            self.invite_open = false;
        } else if self.session_import_open {
            self.session_import_open = false;
            self.session_import_target_chat = None;
        } else if self.annotation_inspector.take().is_some()
            || self.rename_dialog.take().is_some()
            || self.delete_confirm.take().is_some()
            || self.rename_space_dialog.take().is_some()
            || self.delete_space_confirm.take().is_some()
            || self.chat_menu.take().is_some()
            || self.space_menu.take().is_some()
        {
        } else if self.user_menu_open {
            self.user_menu_open = false;
        } else {
            self.composer.update(cx, |composer, cx| {
                composer.interrupt_active(cx);
            });
            return;
        }
        cx.notify();
    }

    fn close_active_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(chat_id) = self.state.read(cx).selected_chat.clone() {
            self.close_session_tab(chat_id, cx);
        } else {
            window.remove_window();
        }
    }

    fn open_annotation_target(&mut self, target_id: String, cx: &mut Context<Self>) {
        let anchor = self
            .state
            .read(cx)
            .collaboration
            .as_ref()
            .and_then(|snapshot| {
                snapshot.publications.iter().rev().find_map(|publication| {
                    let comet_proto::PublicationValue::Annotation(annotation) = &publication.value
                    else {
                        return None;
                    };
                    (annotation.anchor.target_id == target_id).then(|| annotation.anchor.clone())
                })
            })
            .unwrap_or_else(|| Self::whole_message_anchor(target_id));
        self.open_annotation_anchor(anchor, cx);
    }

    fn open_annotation_anchor(
        &mut self,
        anchor: comet_proto::SemanticAnchor,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.read(cx);
        let existing = state.collaboration.as_ref().and_then(|snapshot| {
            snapshot.publications.iter().rev().find_map(|publication| {
                let comet_proto::PublicationValue::Annotation(annotation) = &publication.value
                else {
                    return None;
                };
                (annotation.anchor.target_id == anchor.target_id
                    && annotation.anchor.exact == anchor.exact
                    && annotation.anchor.byte_range == anchor.byte_range)
                    .then(|| annotation.clone())
            })
        });
        let author_subject = state
            .collaboration
            .as_ref()
            .and_then(|snapshot| snapshot.principal.as_ref())
            .map(|principal| principal.subject.clone())
            .unwrap_or_default();
        let session_id = state
            .collaboration
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .message_provenance
                    .iter()
                    .rev()
                    .find(|provenance| provenance.message_id == anchor.target_id)
                    .map(|provenance| provenance.session_id.clone())
            })
            .or_else(|| state.selected_agent_session.clone());
        let is_new = existing.is_none();
        let annotation = existing.unwrap_or_else(|| comet_proto::SemanticAnnotation {
            id: uuid::Uuid::new_v4().to_string(),
            author_subject,
            body: String::new(),
            anchor,
            state: comet_proto::AnnotationState::Anchored,
            created_at: Utc::now().timestamp_millis(),
            resolved_at: None,
            unknown: Default::default(),
        });
        let input = cx.new(|cx| ComposerInput::new("Add a comment…", cx));
        input.update(cx, |input, cx| input.set_text(annotation.body.clone(), cx));
        // Enter (the input's own Submit action) saves like the Comment/Save
        // button. `save_annotation` needs the window, which subscriptions
        // don't carry — the render hook consumes the flag with one in hand.
        let input_sub = cx.subscribe(
            &input,
            |this: &mut Shell, _, event: &ComposerInputEvent, cx| {
                if matches!(
                    event,
                    ComposerInputEvent::Submitted | ComposerInputEvent::QueueSubmitted
                ) {
                    this.annotation_submit_pending = true;
                    cx.notify();
                }
            },
        );
        self.annotation_inspector = Some(AnnotationInspector {
            annotation,
            session_id,
            is_new,
            input,
            error: None,
            popup_origin: None,
            _input_sub: input_sub,
        });
        self.annotation_focus_pending = true;
        self.activity_open = false;
        self.invite_open = false;
        self.command_palette_open = false;
        cx.notify();
    }

    fn whole_message_anchor(message_id: String) -> comet_proto::SemanticAnchor {
        comet_proto::SemanticAnchor {
            target_kind: comet_proto::AnchorTargetKind::Message,
            target_id: message_id,
            file: None,
            byte_range: None,
            exact: None,
            prefix_hash: None,
            suffix_hash: None,
            unknown: Default::default(),
        }
    }

    fn open_selection_annotation(
        &mut self,
        message_id: String,
        exact: String,
        popup_origin: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.open_annotation_anchor(
            comet_proto::SemanticAnchor {
                exact: Some(crate::multiplayer::bounded_anchor_exact(&exact)),
                ..Self::whole_message_anchor(message_id)
            },
            cx,
        );
        if let Some(inspector) = self.annotation_inspector.as_mut() {
            inspector.popup_origin = Some(popup_origin);
        }
    }

    fn append_annotation_to_prompt(
        &mut self,
        annotation: comet_proto::SemanticAnnotation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let context = annotation_prompt_context(&annotation);
        self.composer.update(cx, |composer, cx| {
            composer.append_prompt_context(&context, window, cx)
        });
        self.annotation_inspector = None;
        cx.notify();
    }

    fn annotation_session(&self, cx: &App) -> Option<comet_proto::AgentSessionRecord> {
        let session_id = self.annotation_inspector.as_ref()?.session_id.as_deref()?;
        self.state
            .read(cx)
            .collaboration
            .as_ref()?
            .sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .cloned()
    }

    fn save_annotation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(inspector) = self.annotation_inspector.as_ref() else {
            return;
        };
        let body = inspector.input.read(cx).text().trim().to_string();
        if body.is_empty() {
            if let Some(inspector) = self.annotation_inspector.as_mut() {
                inspector.error = Some("Enter a comment".into());
            }
            cx.notify();
            return;
        }
        let is_new = inspector.is_new;
        let mut annotation = inspector.annotation.clone();
        annotation.body = body.clone();

        if is_new {
            // Ordinary chat runs do not publish an AgentSessionRecord. The
            // comment still belongs in the next prompt; collaboration sessions
            // additionally persist the durable annotation.
            if let Some(session) = self.annotation_session(cx) {
                self.queue_control(
                    session,
                    SessionControlAction::AnnotationCreate {
                        annotation: annotation.clone(),
                    },
                    "Add comment",
                    cx,
                );
            }
            self.append_annotation_to_prompt(annotation, window, cx);
            return;
        }

        let Some(session) = self.annotation_session(cx) else {
            if let Some(inspector) = self.annotation_inspector.as_mut() {
                inspector.error = Some("No agent session".into());
            }
            cx.notify();
            return;
        };
        self.queue_control(
            session,
            SessionControlAction::AnnotationEdit {
                annotation_id: annotation.id,
                body: Some(body),
                anchor: None,
            },
            "Edit comment",
            cx,
        );
        self.annotation_inspector = None;
    }

    fn set_annotation_resolved(&mut self, resolved: bool, cx: &mut Context<Self>) {
        let Some(inspector) = self.annotation_inspector.as_ref() else {
            return;
        };
        let Some(session) = self.annotation_session(cx) else {
            if let Some(inspector) = self.annotation_inspector.as_mut() {
                inspector.error = Some("No agent session".into());
            }
            cx.notify();
            return;
        };
        let annotation_id = inspector.annotation.id.clone();
        self.queue_control(
            session,
            SessionControlAction::AnnotationResolve {
                annotation_id,
                resolved,
            },
            if resolved {
                "Resolve comment"
            } else {
                "Reopen comment"
            },
            cx,
        );
        self.annotation_inspector = None;
    }

    fn session_link(&self, cx: &App) -> Option<String> {
        let chat_id = self.state.read(cx).selected_chat.clone()?;
        self.session_link_for(&chat_id, cx)
    }

    /// The `comet://invite/…` one-click join link for one chat's agent
    /// session, if a currently-valid capability grant names it. The grant id
    /// in the link is routing identity only — command authority is still
    /// verified against the room projection when the invitee joins.
    fn session_link_for(&self, chat_id: &str, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let snapshot = state.collaboration.as_ref()?;
        let principal = snapshot.principal.as_ref()?;
        let session = state
            .selected_agent_session()
            .filter(|session| session.chat_id == chat_id)
            .or_else(|| {
                snapshot
                    .sessions
                    .iter()
                    .find(|session| session.chat_id == chat_id)
            })?;
        let now = Utc::now().timestamp_millis();
        let valid_for_session = |grant: &&comet_proto::CapabilityGrant| {
            grant.principal_subject == principal.subject
                && grant.scope.project_id == principal.project_id
                && grant.scope.session_id.as_deref() == Some(session.session_id.as_str())
                && grant.device_id.as_deref() == Some(session.owner_device_id.as_str())
                && grant.granted_at <= now
                && grant.expires_at.is_some_and(|expires| now < expires)
                && grant.revoked_at.is_none()
        };
        let grant = state
            .selected_invitation_grant
            .as_deref()
            .and_then(|preferred| {
                snapshot
                    .grants
                    .iter()
                    .find(|grant| grant.id == preferred && valid_for_session(grant))
            })
            .or_else(|| snapshot.grants.iter().find(valid_for_session))?;
        comet_proto::CometInvitation::new(chat_id, &session.session_id, &grant.id)
            .map(|invitation| invitation.deep_link())
    }

    fn copy_session_link(&mut self, cx: &mut Context<Self>) {
        let Some(link) = self.session_link(cx) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(link));
        self.link_feedback = Some("Link copied".into());
        cx.notify();
    }

    fn open_session_link(&mut self, cx: &mut Context<Self>) {
        let Some(link) = self.session_link(cx) else {
            return;
        };
        cx.open_url(&link);
        self.link_feedback = Some("Link opened".into());
        cx.notify();
    }

    /// Copy a chat's global session id — the address other sessions use to
    /// reach it (`comet session add/read/send`).
    fn copy_chat_session_id(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        self.chat_menu = None;
        cx.write_to_clipboard(ClipboardItem::new_string(chat_id.to_owned()));
        cx.notify();
    }

    /// Copy a chat's `comet://invite/…` one-click join link, when a live
    /// grant makes one available.
    fn copy_chat_invite_link(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        self.chat_menu = None;
        if let Some(link) = self.session_link_for(chat_id, cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(link));
        }
        cx.notify();
    }

    fn copy_selected_session_id(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(chat_id));
        self.link_feedback = Some("Session ID copied".into());
        cx.notify();
    }

    fn reject_control(
        &mut self,
        action: &'static str,
        actor: SharedString,
        detail: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.control_feedback.push(ControlFeedback {
            command_id: format!("rejected-{}", uuid::Uuid::new_v4()),
            actor,
            action: action.into(),
            occurred_at: Utc::now().timestamp_millis(),
            state: ControlFeedbackState::Rejected,
            detail: Some(detail.into()),
        });
        self.activity_open = true;
        cx.notify();
    }

    fn queue_control(
        &mut self,
        session: comet_proto::AgentSessionRecord,
        action: SessionControlAction,
        action_label: &'static str,
        cx: &mut Context<Self>,
    ) {
        let now = Utc::now().timestamp_millis();
        let required = action.required_capability();
        let reveal_activity = required != comet_proto::CAPABILITY_SESSION_ANNOTATE;
        let resolved = {
            let state = self.state.read(cx);
            let snapshot = state.collaboration.as_ref();
            let principal = snapshot.and_then(|snapshot| snapshot.principal.clone());
            let actor_device_id = state.local_device_id.clone();
            let grant_id = match (snapshot, principal.as_ref(), actor_device_id.as_deref()) {
                (Some(_), Some(principal), Some(actor_device_id))
                    if session.source == comet_proto::AgentSessionSource::Local
                        && session.owner_device_id == actor_device_id
                        && principal.has_capability(required) =>
                {
                    Some(uuid::Uuid::new_v4().to_string())
                }
                (Some(snapshot), Some(_), _) => crate::multiplayer::session_grant_id(
                    snapshot,
                    &session,
                    required,
                    state.selected_invitation_grant.as_deref(),
                    now,
                ),
                _ => None,
            };
            let actor = principal
                .as_ref()
                .map(|principal| state.participant_name(&principal.subject).to_string())
                .unwrap_or_else(|| "You".into());
            let engine = state.engine().cloned();
            (
                principal,
                actor_device_id,
                grant_id,
                actor,
                engine,
                state.selected_chat.clone(),
            )
        };
        let (principal, actor_device_id, grant_id, actor, engine, chat_id) = resolved;
        let actor_label: SharedString = actor.into();
        let Some(principal) = principal else {
            self.reject_control(action_label, actor_label, "Identity unavailable", cx);
            return;
        };
        let Some(actor_device_id) = actor_device_id else {
            self.reject_control(action_label, actor_label, "Device unavailable", cx);
            return;
        };
        let Some(grant_id) = grant_id else {
            self.reject_control(action_label, actor_label, "Control not allowed", cx);
            return;
        };
        let (Some(engine), Some(chat_id)) = (engine, chat_id) else {
            self.reject_control(action_label, actor_label, "Session unavailable", cx);
            return;
        };
        let local_id = format!("pending-{}", uuid::Uuid::new_v4());
        self.control_feedback.push(ControlFeedback {
            command_id: local_id.clone(),
            actor: actor_label,
            action: action_label.into(),
            occurred_at: now,
            state: ControlFeedbackState::Pending,
            detail: None,
        });
        if reveal_activity {
            self.activity_open = true;
        }
        let command = SessionCommandPayload::Control {
            session_id: session.session_id.clone(),
            owner_device_id: session.owner_device_id.clone(),
            actor_device_id,
            actor_subject: principal.subject,
            grant_id,
            source: session.source,
            action: Box::new(action),
        };
        let task = cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::QUEUE_COMMAND,
                    serde_json::json!({
                        "chatId": chat_id,
                        "command": command,
                    }),
                )
                .await;
            this.update(cx, |shell, cx| {
                if let Some(feedback) = shell
                    .control_feedback
                    .iter_mut()
                    .find(|feedback| feedback.command_id == local_id)
                {
                    match result {
                        Ok(value) => {
                            if let Some(command_id) =
                                value.get("commandId").and_then(|value| value.as_str())
                            {
                                feedback.command_id = command_id.to_string();
                            }
                        }
                        Err(error) => {
                            feedback.state = ControlFeedbackState::Rejected;
                            feedback.detail = Some(error.to_string().into());
                        }
                    }
                }
                let audits = shell
                    .state
                    .read(cx)
                    .collaboration
                    .iter()
                    .flat_map(|snapshot| snapshot.publications.iter())
                    .filter_map(|publication| match &publication.value {
                        comet_proto::PublicationValue::Audit(audit) => Some(audit.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                Self::reconcile_control_feedback(&mut shell.control_feedback, &audits);
                cx.notify();
            })
            .ok();
        });
        self.control_tasks.push(task);
        cx.notify();
    }

    fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), cx));
        self.terminal = Some(terminal.clone());
        terminal
    }

    fn terminal_target(&self, cx: &App) -> f32 {
        if self.terminal_open(cx) {
            self.settings.terminal_height
        } else {
            0.0
        }
    }

    /// Cmd/Ctrl+J and the header button (feature-inventory §1.10). Height
    /// animates 200 ms; closing detaches (PTYs stay alive), opening restores.
    /// The flag is per chat (comet `sessionPanels`).
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.terminal_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_terminal(&key);
        self.terminal_tween = Some(WidthTween::new(from, self.terminal_target(cx)));
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| panel.set_open(open, cx));
        if open {
            // Opening lands keyboard focus IN the shell — typing goes straight
            // to the prompt, no click needed (comet terminal-panel.tsx: the
            // visible+active effect calls `terminal.focus()` on every open).
            // The handle is focusable before the panel's first paint; once the
            // terminal body mounts with `track_focus` it receives the keys.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            // Hiding the panel removes the (likely focused) terminal view;
            // with nothing focused, window key bindings stop dispatching, so
            // hand focus to the composer. (Cmd+J is a pure toggle — a second
            // press closes even while the terminal is focused, as in comet's
            // `useHotkey(toggleShortcut, ... setOpenScoped(!open))`.)
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total().mul_f32(motion::speed_scale()) + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        self.settings.terminal_height = clamp_terminal_height(anchor_h + dy, viewport_h);
        self.terminal_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_sidebar_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = f32::from(event.event.position.x);
        self.settings.sidebar_width = x.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = None; // live drag tracks the pointer directly
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_right_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<RightPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        // comet caps the pane at 52% of the window on top of the absolute range.
        let max = RIGHT_PANE_MAX.min(viewport * 0.52);
        self.settings.right_pane_width = width.clamp(RIGHT_PANE_MIN, max.max(RIGHT_PANE_MIN));
        self.right_tween = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// Debounced settings write: waits [`SAVE_DEBOUNCE_MS`], then persists the
    /// latest snapshot on the background executor. Re-scheduling drops (cancels)
    /// the previous timer.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let dir = self.data_dir.clone();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SAVE_DEBOUNCE_MS))
                .await;
            // Re-stamp the appearance from the global before writing. The View
            // menu changes it through `appearance::set_mode`, which never touches
            // this shell's in-memory copy — without this, the next pane resize
            // would quietly write the boot-time appearance back over the user's
            // choice.
            let Ok(snapshot) = this.update(cx, |shell, cx| {
                shell.settings.appearance = crate::appearance::mode(cx);
                shell.settings.clone()
            }) else {
                return;
            };
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = snapshot.save(&dir) {
                        tracing::warn!(error = %err, "failed to persist ui settings");
                    }
                })
                .await;
        }));
    }

    fn retry_engine(&mut self, cx: &mut Context<Self>) {
        AppState::bootstrap(self.state.clone(), self.boot.clone(), cx);
    }

    // ---- routes / settings ----

    fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.route = Route::Settings(section);
        self.nav.push(NavEntry::Settings(section));
        self.user_menu_open = false;
        self.chat_menu = None;
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.nav.push(NavEntry::Chat(self.active_chat.clone()));
        cx.notify();
    }

    // ---- back/forward (route history) ----

    fn navigate_back(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.back() {
            self.apply_nav(entry, cx);
        }
    }

    fn navigate_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.forward() {
            self.apply_nav(entry, cx);
        }
    }

    /// Land on a history entry WITHOUT recording a new one: the stack already
    /// points at `entry` (back/forward moved the index); the selection change
    /// this triggers dedups against `current()` in [`Self::on_state_changed`].
    fn apply_nav(&mut self, entry: NavEntry, cx: &mut Context<Self>) {
        match entry {
            NavEntry::Chat(chat_id) => {
                self.route = Route::Chat;
                let target = (!chat_id.is_empty()).then_some(chat_id);
                if self.state.read(cx).selected_chat != target {
                    self.state.update(cx, |s, cx| s.select_chat(target, cx));
                }
            }
            NavEntry::Settings(section) => {
                self.route = Route::Settings(section);
            }
        }
        self.user_menu_open = false;
        self.chat_menu = None;
        cx.notify();
    }

    /// Lazily create the entity for a settings section and return it renderable.
    fn settings_outlet(&mut self, section: SettingsSection, cx: &mut Context<Self>) -> AnyElement {
        match section {
            SettingsSection::Devices => {
                if self.devices_page.is_none() {
                    let state = self.state.clone();
                    self.devices_page = Some(cx.new(|cx| DevicesPage::new(state, cx)));
                }
                match &self.devices_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Agents => {
                if self.accounts_page.is_none() {
                    let state = self.state.clone();
                    self.accounts_page = Some(cx.new(|cx| AccountsPage::new(state, cx)));
                }
                match &self.accounts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Advisor => {
                if self.advisor_page.is_none() {
                    let state = self.state.clone();
                    self.advisor_page = Some(cx.new(|cx| AdvisorPage::new(state, cx)));
                }
                match &self.advisor_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Appearance => {
                if self.appearance_page.is_none() {
                    self.appearance_page = Some(cx.new(AppearancePage::new));
                }
                match &self.appearance_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Shortcuts => {
                if self.shortcuts_page.is_none() {
                    let state = self.state.clone();
                    let keymap = self.settings.keymap.clone();
                    let page = cx.new(|cx| ShortcutsPage::new(state, keymap, cx));
                    // Persist + re-apply the keymap whenever the page changes it.
                    self.shortcuts_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &ShortcutsEvent, cx| {
                            let ShortcutsEvent::Changed(keymap) = event;
                            this.settings.keymap = keymap.clone();
                            apply_keymap(cx, keymap);
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.shortcuts_page = Some(page);
                }
                match &self.shortcuts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Archived => {
                if self.archived_page.is_none() {
                    let state = self.state.clone();
                    self.archived_page = Some(cx.new(|cx| ArchivedPage::new(state, cx)));
                }
                match &self.archived_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
        }
    }

    // ---- sidebar mutations ----

    /// Fire a Mutate op; failures surface in the sidebar notice strip.
    fn mutate(&mut self, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sidebar_notice = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(format!("{err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }
    /// Drop an imported membership pin (the ✕ on a Shared row). One-click
    /// invite links are the only path that creates these pins now.
    fn remove_shared_session(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sidebar_notice = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let state = self.state.clone();
        self.session_ref_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REMOVE_SESSION_REF,
                    serde_json::json!({ "chatId": chat_id.clone() }),
                )
                .await;
            match result {
                Ok(_) => {
                    state.update(cx, |state, cx| {
                        let mut refs = state.session_refs.clone();
                        refs.retain(|item| item.chat_id != chat_id);
                        state.apply_session_refs(refs);
                        cx.notify();
                    });
                }
                Err(err) => {
                    this.update(cx, |shell, cx| {
                        shell.sidebar_notice = Some(format!("Remove failed: {err}").into());
                        cx.notify();
                    })
                    .ok();
                }
            }
        }));
    }

    fn open_rename_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        let current = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Session title", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_chat(cx);
            }
        });
        self.rename_dialog = Some(RenameChatDialog {
            chat_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename_chat(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let title = dialog.input.read(cx).text().trim().to_string();
        if !title.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameChat", "chatId": dialog.chat_id, "title": title }),
                cx,
            );
        }
        cx.notify();
    }

    /// Settle (archive) or unsettle a session. Archiving an attached Scaffold
    /// chat pauses its exact sandbox only after the durable chat mutation wins.
    fn set_chat_settled(&mut self, chat_id: String, settled: bool, cx: &mut Context<Self>) {
        self.chat_menu = None;
        let scaffold_target = settled
            .then(|| {
                self.state
                    .read(cx)
                    .scaffold_control_target(&chat_id)
                    .cloned()
            })
            .flatten();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sidebar_notice = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let result = if let Some(target) = scaffold_target {
                crate::state::archive_and_pause_scaffold_session(&engine, &chat_id, &target).await
            } else {
                engine
                    .client()
                    .call(
                        methods::MUTATE,
                        serde_json::json!({
                            "op": "setChatArchived",
                            "chatId": chat_id,
                            "archived": settled,
                        }),
                    )
                    .await
                    .map(|_| ())
            };
            if let Err(error) = result {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(error.to_string().into());
                    cx.notify();
                })
                .ok();
            }
        });
        self.control_tasks.push(task);
        cx.notify();
    }

    fn delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str()) {
            self.state.update(cx, |s, cx| s.select_chat(None, cx));
        }
        self.composer
            .update(cx, |composer, _| composer.purge_chat(&chat_id));
        self.mutate(
            serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
            cx,
        );
        cx.notify();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.user_menu_open = false;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine
                .client()
                .call(methods::SIGN_OUT, serde_json::json!({}))
                .await
            {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(format!("Sign out failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
        cx.notify();
    }

    fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SIGN_IN, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| match result {
                Ok(value) => {
                    if let Some(url) = value.get("url").and_then(|u| u.as_str()) {
                        cx.open_url(url);
                    }
                }
                Err(err) => {
                    shell.sidebar_notice = Some(format!("Sign in failed: {err}").into());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    // ---- render pieces ----

    /// Evaluate a width tween at "now" (manual drive — see [`WidthTween`]).
    /// Mid-flight: eased 200ms lerp, and `motion_active` is flagged so render
    /// schedules the next animation frame. Finished, stale, absent, or under
    /// reduced motion: exactly `target`. Honors `COMET_MOTION_SCALE`.
    fn eval_tween(&self, tween: Option<WidthTween>, target: f32) -> f32 {
        let Some(WidthTween { from, to, started }) = tween else {
            return target;
        };
        if self.reduced_motion {
            return target;
        }
        let total = RESIZE.total().mul_f32(motion::speed_scale());
        let raw = started.elapsed().as_secs_f32() / total.as_secs_f32();
        if raw >= 1.0 {
            return target;
        }
        self.motion_active.set(true);
        motion::lerp(from, to, RESIZE.progress(raw))
    }

    /// Animated width container: tweens 200ms ease-out on collapse/expand, and
    /// clips a fixed-width inner so content never reflows mid-transition.
    fn pane_container(
        &self,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        div()
            .h_full()
            .flex_none()
            .overflow_hidden()
            .w(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// The animated spacer clearing the macOS traffic lights ahead of a
    /// titlebar control cluster. Fullscreen toggles tween the cluster start
    /// over 200ms ease-out ([`RESIZE`]; reduced motion snaps).
    /// `None` off macOS — no phantom flex child.
    fn titlebar_spacer(&self, container_pad: f32) -> Option<AnyElement> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let fullscreen = self.fullscreen.unwrap_or(false);
        // The tween runs in cluster-start coordinates; the spacer is that
        // minus the container's own padding.
        let start = self.eval_tween(self.titlebar_tween, titlebar_cluster_start(fullscreen));
        let width = (start - container_pad).max(0.0);
        Some(div().flex_none().h_full().w(px(width)).into_any_element())
    }

    /// The header's content row with the animated left inset — the native port
    /// of comet __root.tsx `transition-[padding-left] duration-200 ease-out` +
    /// `style={{ paddingLeft: headerInset }}`: on sidebar toggles (and macOS
    /// fullscreen flips) the SAME element's padding tweens, so the title
    /// glides to its new x-position. Route changes SNAP: the tween is killed
    /// by every route transition (comet remounts the keyed header variants —
    /// instant swap, zero horizontal motion).
    /// Where unified-titlebar content (tabs / the settings label) starts: past
    /// the traffic lights + control cluster, riding the fullscreen inset tween.
    pub(super) fn title_bar_content_start(&self) -> f32 {
        let fullscreen = self.fullscreen.unwrap_or(false);
        let is_macos = cfg!(target_os = "macos");
        let cluster = self.eval_tween(
            self.titlebar_tween,
            cluster_buttons_start(is_macos, fullscreen),
        );
        cluster + CLUSTER_BUTTONS_WIDTH + 10.0
    }

    /// The unified window titlebar: chat → the session tab strip; settings →
    /// the section label. Full-width on the glass shell; the traffic lights
    /// and control cluster overlay its left end.
    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.route {
            Route::Chat => self.render_session_tab_strip(cx),
            Route::Settings(_) => {
                let inner = div()
                    .size_full()
                    .flex()
                    .items_center()
                    .pt(px(Theme::TITLEBAR_TOP_PAD))
                    .pl(px(self.title_bar_content_start()))
                    .pr(px(titlebar_right_padding(
                        cfg!(target_os = "windows"),
                        Theme::SPACE_LG,
                    )));
                let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
                self.titlebar_drag_region("settings-header-titlebar", bar, cx)
                    .into_any_element()
            }
        }
    }

    /// Make a titlebar strip drag the window: mark it a
    /// [`WindowControlArea::Drag`] target, hand the drag to the compositor once
    /// the pointer moves with the button down, and double-click to zoom.
    fn titlebar_drag_region(
        &self,
        id: &'static str,
        el: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.id(id)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.titlebar_should_move = false))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = false),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = true),
            )
            // Hand the drag to the compositor only while the button is
            // actually held (`pressed_button` guard): on macOS
            // `start_window_move` runs AppKit's NATIVE drag session
            // (`performWindowDragWithEvent:`), and AppKit resolves a quick
            // second click inside that session as a titlebar double-click —
            // system zoom — natively, beyond gpui's reach. Without the guard a
            // stale `titlebar_should_move` (armed by a down whose bubble was
            // later stopped) would start that session from a mere hover move
            // between the two clicks of a double-click.
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.titlebar_should_move && event.pressed_button == Some(MouseButton::Left)
                    {
                        this.titlebar_should_move = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    if cfg!(target_os = "macos") {
                        // Native titlebar double-click action (zoom/minimize
                        // per system preference).
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
                }
            })
    }

    /// The ONE top-left window-control cluster (sidebar toggle + back/forward —
    /// comet window-controls.tsx): rendered once, in a paint-only overlay layer
    /// pinned at the window's top-left, ABOVE the sidebar and headers. The
    /// sidebar width animates *beneath* it, so the buttons keep their element
    /// identity and never move or remount on collapse/expand; only the
    /// fullscreen traffic-light inset tweens (the animated spacer). The
    /// container has no id/listeners — everything between the buttons falls
    /// through to the titlebar drag strips below.
    fn render_titlebar_cluster(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let can_back = self.nav.can_back();
        let can_forward = self.nav.can_forward();
        div()
            .absolute()
            .top_0()
            .left_0()
            .h(px(Theme::TITLEBAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(2.0))
            .px(px(10.0))
            .children(self.titlebar_spacer(12.0))
            .child(window_control_button(
                "toggle-sidebar",
                icons::SIDEBAR_MINIMALISTIC_LEFT,
                &theme,
                cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
            ))
            .child(nav_history_button(
                "nav-back",
                icons::ARROW_LEFT,
                can_back,
                &theme,
                cx.listener(|this, _, _, cx| this.navigate_back(cx)),
            ))
            .child(nav_history_button(
                "nav-forward",
                icons::ARROW_RIGHT,
                can_forward,
                &theme,
                cx.listener(|this, _, _, cx| this.navigate_forward(cx)),
            ))
            .into_any_element()
    }

    /// Native Windows caption controls integrated into Comet's unified
    /// titlebar. `WindowControlArea` maps these hit targets to HTMINBUTTON,
    /// HTMAXBUTTON, and HTCLOSE, so Windows owns their behavior (including
    /// Snap Layouts) while GPUI renders the system Segoe caption glyphs.
    fn render_windows_caption_controls(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        if !cfg!(target_os = "windows") {
            return None;
        }

        let theme = Theme::of(cx);
        let (maximize_id, maximize_glyph) = if window.is_maximized() {
            ("window-restore", "\u{e923}")
        } else {
            ("window-maximize", "\u{e922}")
        };
        Some(
            div()
                .id("windows-window-controls")
                .absolute()
                .top_0()
                .right_0()
                .h(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_row()
                .font_family("Segoe Fluent Icons")
                .child(windows_caption_button(
                    "window-minimize",
                    "\u{e921}",
                    WindowControlArea::Min,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    maximize_id,
                    maximize_glyph,
                    WindowControlArea::Max,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    "window-close",
                    "\u{e8bb}",
                    WindowControlArea::Close,
                    theme,
                    true,
                ))
                .into_any_element(),
        )
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let inner: AnyElement = match self.route {
            Route::Settings(section) => self.render_settings_nav(section, &theme, cx),
            Route::Chat => self.render_chat_sidebar(&theme, cx),
        };
        let target = self.sidebar_target();
        // Transparent — the sidebar sits directly on the frost shell; the main
        // card's own border provides the separation.
        self.pane_container(
            self.sidebar_tween,
            target,
            div().h_full().child(inner).into_any_element(),
        )
    }

    /// Settings-mode sidebar (comet settings-sidebar.tsx): window-control
    /// strip, "Settings" heading, icon section rows styled like session rows,
    /// and a Back row pinned to the bottom.
    fn render_settings_nav(
        &mut self,
        section: SettingsSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section_icon = |item: SettingsSection| match item {
            SettingsSection::Devices => icons::MONITOR,
            SettingsSection::Agents => icons::KEY_MINIMALISTIC,
            SettingsSection::Advisor => icons::CHAT_ROUND_LINE,
            SettingsSection::Appearance => icons::TUNING,
            SettingsSection::Shortcuts => icons::KEYBOARD,
            SettingsSection::Archived => icons::ARCHIVE_MINIMALISTIC,
        };
        // Match the user's dragged sidebar width — the pane container clips to
        // it, so a hardcoded default here left hover washes stopping short of
        // the sidebar's right edge (user-reported). Device identity lives on
        // the Accounts page now — the one surface where the device matters.
        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .px(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(Theme::SPACE_SM))
                            .pt(px(12.0))
                            .pb(px(4.0))
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from("Settings")),
                    )
                    .child(div().flex().flex_col().gap(px(2.0)).children(
                        SettingsSection::ALL.into_iter().map(|item| {
                            let selected = item == section;
                            div()
                                .id(SharedString::from(format!("settings-nav-{}", item.label())))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .rounded(px(8.0))
                                .px(px(Theme::SPACE_SM))
                                .py(px(6.0))
                                .text_size(px(13.0))
                                .when(selected, |el| {
                                    el.bg(crate::theme::wash(0.17))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                })
                                .text_color(if selected {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })
                                .cursor_pointer()
                                .hover(|s| s.bg(crate::theme::wash(0.11)).text_color(theme.text))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_settings(item, cx)),
                                )
                                .child(
                                    icon(section_icon(item))
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from(item.label()))
                        }),
                    )),
            )
            // Back pinned to the bottom (comet settings-sidebar.tsx).
            .child(
                div().px(px(Theme::SPACE_SM)).pb(px(12.0)).child(
                    div()
                        .id("settings-back")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(8.0))
                        .px(px(Theme::SPACE_SM))
                        .py(px(6.0))
                        .text_size(px(13.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|s| s.bg(crate::theme::wash(0.11)).text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
                        .child(
                            // AltArrowLeft chevron (comet settings-sidebar.tsx),
                            // not the straight history arrow.
                            icon(icons::ALT_ARROW_LEFT)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Back")),
                ),
            )
            .into_any_element()
    }

    /// Rich session row: room context, title, source/runtime/model, and the
    /// top-right status slot — a spinner while the agent works, an attention
    /// dot when it finished or needs input, otherwise the recency label.
    /// Hovering the row cross-fades that read-out out and a settle checkmark in
    /// (`settled` tints it green, so the settled list unsettles). The row
    /// retains the same content at both density settings; compact only
    /// tightens the vertical insets.
    #[allow(clippy::too_many_arguments)]
    fn render_chat_row(
        &self,
        id: String,
        title: SharedString,
        time_ago: SharedString,
        space_name: SharedString,
        branch: Option<SharedString>,
        meta: SidebarSessionMeta,
        status: comet_proto::ChatIndicator,
        settled: bool,
        selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let subline = theme.text_muted.opacity(0.66);
        let history_source = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == id)
            .and_then(|chat| {
                imported_chat_history_source(&chat.id, chat.harness_session_id.as_deref())
            });
        let source_label =
            history_source.unwrap_or_else(|| crate::multiplayer::source_label(meta.source));
        let compact = self.settings.density == Density::Compact;
        let (hover, text) = (theme.glass_hover(), theme.text);
        let selected_wash = crate::theme::glass_selected_bg();
        let select_id = id.clone();
        let menu_id = id.clone();
        let fade_key = format!("chat-row-{id}");
        let rest_bg = if selected {
            selected_wash
        } else {
            crate::theme::wash(0.0)
        };
        let hover_bg = if selected { selected_wash } else { hover };
        let rest_text = if selected { text } else { text.opacity(0.8) };
        // Top-right status slot (reference shot): Working spins the thin arc,
        // finished-unseen / awaiting-input / errored raise the attention dot,
        // Idle keeps the plain recency label.
        let status_slot: AnyElement = match status {
            // 9px ring nudged 1px down: its top edge lands 3px into the 13px
            // band — the same inset as the attention dot — instead of hugging
            // the card's top border (user report).
            comet_proto::ChatIndicator::Working => div()
                .mt(px(1.0))
                .child(crate::loaders::arc_spinner(
                    theme.text_muted.opacity(0.8),
                    9.0,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            // One attention blue for every finished/needs-you state (reference
            // shot) — errored included; the transcript's error chip carries
            // the red. 7px in the 13px slot keeps whole-pixel centering.
            comet_proto::ChatIndicator::AwaitingInput
            | comet_proto::ChatIndicator::Completed
            | comet_proto::ChatIndicator::Errored => div()
                .size(px(7.0))
                .rounded_full()
                .bg(theme.attention)
                .into_any_element(),
            comet_proto::ChatIndicator::Idle => div()
                .text_size(px(10.5))
                .line_height(px(13.0))
                .text_color(subline)
                .child(time_ago)
                .into_any_element(),
        };
        // Hover reveal without a `group-hover` (gpui has none): the row's own
        // wash fade — already driven by the `on_hover` below — doubles as the
        // reveal progress, so the swap rides the same 150ms curve as the wash
        // and needs no state of its own. Built only while that fade is off
        // rest, so the many resting rows carry no extra hitbox or tooltip.
        let reveal = motion::hover_t(&fade_key);
        let settle_id = id.clone();
        let settle_button: Option<AnyElement> = (reveal > 0.0).then(|| {
            div()
                .id(SharedString::from(format!("settle-{id}")))
                // An 18px rounded hit target reads as an action, not a form
                // checkbox. It overlays the 13px status band without reflow.
                .size(px(18.0))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(5.0))
                .bg(if settled {
                    theme.success.opacity(0.10)
                } else {
                    crate::theme::ink(0.06)
                })
                .hover(|el| el.bg(crate::theme::ink(0.13)))
                .cursor_pointer()
                .tooltip(popover::text_tooltip(if settled {
                    "Unsettle session"
                } else {
                    "Settle session"
                }))
                // The row underneath is clickable (select); see [`SettlePress`]
                // for why the press — not `stop_propagation` — arbitrates.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, _| {
                        this.settle_press.press_button();
                    }),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_chat_settled(settle_id.clone(), !settled, cx);
                }))
                .child(icon(icons::CHECK).size(px(12.0)).text_color(if settled {
                    theme.success
                } else {
                    theme.text_muted.opacity(0.82)
                }))
                .into_any_element()
        });
        let link_actions = (reveal > 0.0
            && (meta.scaffold_web.is_some() || meta.scaffold_session.is_some()))
        .then(|| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(2.0))
                .opacity(reveal)
                .when_some(meta.scaffold_web.clone(), |actions, link| {
                    actions.child(
                        div()
                            .id(SharedString::from(format!("scaffold-web-{id}")))
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .hover(|el| el.bg(crate::theme::ink(0.10)))
                            .tooltip(popover::text_tooltip("Open web"))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.open_url(&link);
                            }))
                            .child(
                                icon(icons::GLOBAL)
                                    .size(px(11.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                })
                .when_some(meta.scaffold_session.clone(), |actions, link| {
                    actions.child(
                        div()
                            .id(SharedString::from(format!("scaffold-session-{id}")))
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .hover(|el| el.bg(crate::theme::ink(0.10)))
                            .tooltip(popover::text_tooltip("Open session"))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.open_url(&link);
                            }))
                            .child(
                                icon(icons::MONITOR)
                                    .size(px(11.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                })
                .into_any_element()
        });
        div()
            .id(SharedString::from(format!("chat-{id}")))
            .flex()
            .flex_col()
            .gap(px(if compact { 1.0 } else { 2.0 }))
            .rounded(px(Theme::CONTROL_RADIUS))
            .px(px(Theme::SPACE_SM))
            .py(px(if compact {
                Theme::SPACE_XS
            } else {
                Theme::SPACE_SM
            }))
            .text_color(motion::hover_blend(&fade_key, rest_text, text))
            .bg(motion::hover_blend(&fade_key, rest_bg, hover_bg))
            .when(selected, |el| {
                el.shadow(crate::theme::glass_selected_shadows())
            })
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            // Claims the checkbox press raised just above (a descendant's press
            // listener runs first), for EVERY left press the row contains — so
            // no press is ever attributed to a later click ([`SettlePress`]).
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, _| {
                    this.settle_press.press_row();
                }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.settle_press.row_click_selects() {
                    return;
                }
                let id = select_id.clone();
                this.state
                    .update(cx, |state, cx| state.select_chat(Some(id), cx));
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.chat_menu = Some((menu_id.clone(), event.position));
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.5))
                            .line_height(px(13.0))
                            .text_color(subline)
                            .child(space_name),
                    )
                    .child(
                        // Fixed to the header line's 13px so dot/spinner/label
                        // all center on the same baseline band. The button
                        // rides an absolute overlay pinned to the band's right
                        // edge: the read-out keeps owning the slot's width, so
                        // the swap costs no reflow at any indicator width.
                        div()
                            .flex_none()
                            .h(px(13.0))
                            .relative()
                            .flex()
                            .items_center()
                            .child(div().opacity(1.0 - reveal).child(status_slot))
                            .when_some(settle_button, |el, button| {
                                el.child(
                                    div()
                                        .absolute()
                                        .top(px(-2.5))
                                        .right(px(0.0))
                                        .opacity(reveal)
                                        .child(button),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(13.0))
                    .line_height(px(17.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(title),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_XS))
                    .child(
                        div()
                            .h(px(17.0))
                            .px(px(5.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .bg(crate::theme::ink(0.07))
                            .flex()
                            .items_center()
                            .text_size(px(9.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(SharedString::from(source_label)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(subline)
                            .child(meta.runtime_model),
                    )
                    .when_some(branch, |el, branch| {
                        el.child(icon(icons::GIT_BRANCH).size(px(10.0)).text_color(subline))
                            .child(
                                div()
                                    .max_w(px(72.0))
                                    .truncate()
                                    .text_size(px(10.0))
                                    .text_color(subline)
                                    .child(branch),
                            )
                    })
                    .when_some(link_actions, |el, actions| el.child(actions)),
            )
            .into_any_element()
    }
    /// Imported memberships without a workspace chat row — globe rows carrying
    /// the transcript-learned title. They join the main Sessions list (keyed
    /// for its resort FLIP); row-backed sessions render as normal chat rows.
    fn render_shared_rows(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<(String, f32, AnyElement)> {
        /// Estimated row height feeding the resort glide (py 6×2 + two lines).
        const SHARED_ROW_HEIGHT: f32 = 45.0;
        let now = Utc::now();
        let selected = self.state.read(cx).selected_chat.clone();
        let rows: Vec<(String, SharedString, SharedString)> = {
            let state = self.state.read(cx);
            state
                .shared_session_refs()
                .map(|session_ref| {
                    (
                        session_ref.chat_id.clone(),
                        state.shared_session_title(&session_ref.chat_id).into(),
                        format_time_ago(session_ref.added_at, now).into(),
                    )
                })
                .collect()
        };
        rows.into_iter()
            .map(|(chat_id, title, added)| {
                let key = format!("g:{chat_id}");
                let select_id = chat_id.clone();
                let remove_id = chat_id.clone();
                let is_selected = selected.as_deref() == Some(chat_id.as_str());
                let rest_bg = if is_selected {
                    crate::theme::glass_selected_bg()
                } else {
                    crate::theme::wash(0.0)
                };
                let element = div()
                    .id(SharedString::from(format!("shared-{chat_id}")))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(8.0))
                    .px(px(Theme::SPACE_SM))
                    .py(px(6.0))
                    .bg(rest_bg)
                    .hover(|el| el.bg(theme.glass_hover()))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.state.update(cx, |state, cx| {
                            state.select_chat(Some(select_id.clone()), cx)
                        });
                    }))
                    .child(
                        icon(icons::GLOBAL)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(13.0))
                                    .text_color(theme.text.opacity(0.85))
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_faint)
                                    .child(added),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("remove-shared-{chat_id}")))
                            .size(px(24.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .hover(|el| el.bg(theme.glass_hover()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_shared_session(remove_id.clone(), cx);
                            }))
                            .child(
                                icon(icons::CLOSE)
                                    .size(px(11.0))
                                    .text_color(theme.text_faint),
                            ),
                    )
                    .into_any_element();
                (key, SHARED_ROW_HEIGHT, element)
            })
            .collect()
    }

    fn render_local_session_candidate_row(
        &self,
        candidate: comet_proto::LocalSessionCandidate,
        attaching: bool,
        error: Option<String>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let capability =
            LocalSessionCapability::from_flags(candidate.resumable, candidate.history_only);
        let busy_elsewhere = candidate.busy_elsewhere == Some(true);
        let runtime_model: SharedString = crate::multiplayer::runtime_model(
            Some(crate::multiplayer::harness_label(candidate.harness)),
            candidate.model.as_deref(),
        )
        .into();
        let age: SharedString = local_session_age(candidate.updated_at, Utc::now()).into();
        let title: SharedString = transcript::single_line(&candidate.title).into();
        let context: SharedString = candidate.cwd.clone().into();
        let compact = self.settings.density == Density::Compact;
        let subline = theme.text_muted.opacity(0.66);
        let row_id = candidate.id.clone();
        let target_chat_id = candidate.chat_id.clone();
        let fade_key = format!("local-session-row-{}", candidate.id);
        let rest_bg = crate::theme::wash(0.0);
        let hover_bg = theme.glass_hover();
        let action_text = div()
            .text_size(px(9.5))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted)
            .child(SharedString::from(if attaching {
                match capability {
                    LocalSessionCapability::Resume => "Opening…",
                    LocalSessionCapability::ImportHistory => "Importing…",
                    LocalSessionCapability::Unavailable => "Unavailable",
                }
            } else {
                capability.action_label(busy_elsewhere)
            }));
        let action_text = if attaching {
            let delta = motion::pulse_delta(&motion::COMET_PULSE, cx.entity_id(), cx);
            action_text.opacity(0.56 + 0.44 * delta).into_any_element()
        } else {
            action_text.into_any_element()
        };

        div()
            .id(SharedString::from(format!(
                "local-session-{}",
                candidate.id
            )))
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(if compact { 1.0 } else { 2.0 }))
            .rounded(px(Theme::CONTROL_RADIUS))
            .px(px(Theme::SPACE_SM))
            .py(px(if compact {
                Theme::SPACE_XS
            } else {
                Theme::SPACE_SM
            }))
            .text_color(motion::hover_blend(
                &fade_key,
                theme.text.opacity(0.8),
                theme.text,
            ))
            .bg(motion::hover_blend(&fade_key, rest_bg, hover_bg))
            .when(capability.can_attach() && !attaching, |row| {
                row.on_hover(motion::hover_listener(fade_key))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.session_import_target_chat = Some(target_chat_id.clone());
                        this.state.update(cx, |state, cx| {
                            state.attach_local_session(row_id.clone(), cx);
                        });
                        cx.notify();
                    }))
            })
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.5))
                            .line_height(px(13.0))
                            .text_color(subline)
                            .child(context),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(subline)
                            .child(age),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .line_height(px(17.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(title),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_XS))
                    .child(
                        div()
                            .flex_none()
                            .h(px(17.0))
                            .px(px(5.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .bg(crate::theme::ink(0.07))
                            .flex()
                            .items_center()
                            .text_size(px(9.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(SharedString::from(capability.source_label(busy_elsewhere))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(subline)
                            .child(runtime_model),
                    )
                    .child(
                        div()
                            .flex_none()
                            .h(px(17.0))
                            .px(px(5.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .bg(crate::theme::ink(0.07))
                            .flex()
                            .items_center()
                            .child(action_text),
                    ),
            )
            .when_some(error, |row, error| {
                row.child(
                    div()
                        .pt(px(Theme::SPACE_XS))
                        .text_size(px(10.5))
                        .line_height(px(14.0))
                        .text_color(theme.danger)
                        .child(SharedString::from(error)),
                )
            })
            .into_any_element()
    }

    fn render_local_session_provider_row(
        &mut self,
        harness: comet_proto::HarnessId,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(candidate) = self
            .session_import_sections
            .iter()
            .find(|section| section.harness == harness)
            .and_then(|section| section.sessions.get(index))
            .cloned()
        else {
            return Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let (attaching, error) = {
            let state = self.state.read(cx);
            (
                state.local_session_attaching.contains(&candidate.id),
                state
                    .local_session_attach_errors
                    .get(&candidate.id)
                    .cloned(),
            )
        };

        div()
            .h(px(LOCAL_SESSION_PROVIDER_ROW_HEIGHT))
            .px(px(Theme::SPACE_SM))
            .child(self.render_local_session_candidate_row(candidate, attaching, error, &theme, cx))
            .into_any_element()
    }

    fn render_local_session_provider_section(
        &mut self,
        section: LocalSessionProviderSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let harness = section.harness;
        let count = section.sessions.len();
        let Some(list_state) = self.session_import_lists.get(&harness).cloned() else {
            return Empty.into_any_element();
        };
        let fold = self
            .session_import_folds
            .get(&harness)
            .copied()
            .unwrap_or_default();
        let expanded_height = local_session_provider_viewport_height(count);
        let target_height = if fold.expanded { expanded_height } else { 0.0 };
        let provider_id = crate::multiplayer::harness_label(harness)
            .to_ascii_lowercase()
            .replace(' ', "-");
        let chevron_icon = if fold.expanded {
            icons::ALT_ARROW_DOWN
        } else {
            icons::ALT_ARROW_RIGHT
        };
        let chevron = div().flex_none().size(px(14.0)).child(
            icon(chevron_icon)
                .size(px(13.0))
                .text_color(theme.text_muted.opacity(0.7)),
        );
        let chevron: AnyElement = if fold.animating() {
            chevron
                .with_animation(
                    SharedString::from(format!(
                        "local-session-provider-chevron-{provider_id}-{}",
                        fold.epoch
                    )),
                    motion::CHEVRON.animation(),
                    |el, t| el.opacity(0.25 + 0.75 * t),
                )
                .into_any_element()
        } else {
            chevron.into_any_element()
        };
        let header = div()
            .id(SharedString::from(format!(
                "local-session-provider-{provider_id}"
            )))
            .h(px(LOCAL_SESSION_PROVIDER_HEADER_HEIGHT))
            .w_full()
            .flex_none()
            .px(px(Theme::SPACE_MD))
            .border_b_1()
            .border_color(theme.border.opacity(0.72))
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_XS))
            .cursor_pointer()
            .hover(|style| style.bg(theme.element_hover))
            .text_size(px(10.5))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(theme.text_muted)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_session_import_provider(harness);
                cx.notify();
            }))
            .child(chevron)
            .child(SharedString::from(crate::multiplayer::harness_label(
                harness,
            )))
            .child(
                div()
                    .font_weight(gpui::FontWeight::NORMAL)
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!(
                        "{count} session{}",
                        if count == 1 { "" } else { "s" }
                    ))),
            );
        let wheel_list_state = list_state.clone();
        let rows = list(
            list_state,
            cx.processor(move |this, index: usize, window, cx| {
                this.render_local_session_provider_row(harness, index, window, cx)
            }),
        )
        .size_full()
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto);
        let body_content = div()
            .h(px(expanded_height))
            .w_full()
            .min_w_0()
            .on_scroll_wheel(move |event, window, cx| {
                let desired_delta = -event.delta.pixel_delta(px(20.0)).y;
                let distance = local_session_provider_scroll_distance(
                    count,
                    wheel_list_state.logical_scroll_top(),
                    desired_delta,
                );
                wheel_list_state.scroll_by(distance);
                window.refresh();
                cx.stop_propagation();
            })
            .child(rows);
        let body: AnyElement = if fold.animating() {
            let from = fold.from;
            div()
                .overflow_hidden()
                .child(body_content)
                .with_animation(
                    SharedString::from(format!(
                        "local-session-provider-fold-{provider_id}-{}",
                        fold.epoch
                    )),
                    motion::COLLAPSE.animation(),
                    move |el, t| el.h(px(motion::lerp(from, target_height, t))),
                )
                .into_any_element()
        } else {
            div()
                .h(px(target_height))
                .overflow_hidden()
                .child(body_content)
                .into_any_element()
        };

        div()
            .w_full()
            .min_w_0()
            .flex_none()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_session_import_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let (count, loading, error) = {
            let state = self.state.read(cx);
            (
                state.local_session_candidates.len(),
                state.local_sessions_loading,
                state.local_sessions_error.clone(),
            )
        };
        let content = if count > 0 {
            let mut provider_sections = Vec::with_capacity(self.session_import_sections.len());
            for section in self.session_import_sections.clone() {
                provider_sections
                    .push(self.render_local_session_provider_section(section, &theme, cx));
            }

            div()
                .id("local-session-provider-groups")
                .h(px(LOCAL_SESSION_IMPORT_CONTENT_HEIGHT))
                .w_full()
                .min_w_0()
                .overflow_y_scroll()
                .track_scroll(&self.session_import_groups_scroll)
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .children(provider_sections),
                )
                .into_any_element()
        } else if loading {
            div()
                .h(px(LOCAL_SESSION_IMPORT_CONTENT_HEIGHT))
                .px(px(Theme::SPACE_SM))
                .pt(px(Theme::SPACE_SM))
                .child(popover::skeleton_rows(
                    "local-session-import-skeleton",
                    &theme,
                    6,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element()
        } else if let Some(error) = error.as_ref() {
            div()
                .h(px(LOCAL_SESSION_IMPORT_CONTENT_HEIGHT))
                .px(px(Theme::SPACE_MD))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(Theme::SPACE_SM))
                .text_size(px(12.0))
                .text_color(theme.danger)
                .child(SharedString::from(error.clone()))
                .child(
                    popover::btn_ghost(&theme, "Retry", "local-session-import-retry")
                        .id("local-session-import-retry")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.state.update(cx, |state, cx| {
                                state.load_local_sessions(true, cx);
                            });
                        })),
                )
                .into_any_element()
        } else {
            div()
                .h(px(LOCAL_SESSION_IMPORT_CONTENT_HEIGHT))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No local sessions found"))
                .into_any_element()
        };
        let card = popover::dialog_card(&theme)
            .w(px(LOCAL_SESSION_IMPORT_DIALOG_WIDTH))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.session_import_open = false;
                    this.session_import_target_chat = None;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .px(px(Theme::SPACE_MD))
                    .pt(px(Theme::SPACE_MD))
                    .pb(px(Theme::SPACE_SM))
                    .child(popover::dialog_title(&theme, "Import session"))
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_size(px(12.0))
                            .line_height(px(16.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(
                                "Open an existing local session from a supported harness.",
                            )),
                    ),
            )
            .when(count > 0, |el| {
                el.when_some(error.clone(), |el, error| {
                    el.child(
                        div()
                            .mx(px(Theme::SPACE_MD))
                            .mb(px(Theme::SPACE_XS))
                            .text_size(px(11.0))
                            .text_color(theme.danger)
                            .child(SharedString::from(error)),
                    )
                })
            })
            .child(content)
            .child(
                div()
                    .px(px(Theme::SPACE_MD))
                    .py(px(Theme::SPACE_SM))
                    .border_t_1()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(
                                "Claude Code, Codex, OMP, Prime Agent, and OpenCode",
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_XS))
                            .child(
                                popover::btn_ghost(
                                    &theme,
                                    if loading { "Refreshing…" } else { "Refresh" },
                                    "local-session-import-refresh",
                                )
                                .id("local-session-import-refresh")
                                .when(!loading, |button| {
                                    button.on_click(cx.listener(|this, _, _, cx| {
                                        this.state.update(cx, |state, cx| {
                                            state.load_local_sessions(true, cx);
                                        });
                                    }))
                                }),
                            )
                            .child(
                                popover::btn_ghost(&theme, "Close", "local-session-import-close")
                                    .id("local-session-import-close")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.session_import_open = false;
                                        this.session_import_target_chat = None;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element();
        popover::dismissible_modal(
            "local-session-import-dialog",
            viewport,
            card,
            cx.listener(|this, _, _, cx| {
                this.session_import_open = false;
                this.session_import_target_chat = None;
                cx.notify();
            }),
        )
    }

    /// Which sidebar-list edges have hidden overflow (offset from the LAST
    /// frame — the invisible one-frame lag every fade here rides).
    pub(super) fn sidebar_fade_zones(&self) -> (bool, bool) {
        let scrolled = -f32::from(self.sidebar_scroll.offset().y);
        let max_scroll = f32::from(self.sidebar_scroll.max_offset().y);
        (scrolled > 1.0, scrolled < max_scroll - 1.0)
    }

    /// Chat-mode sidebar (spaces overhaul): window-control strip, the Spaces
    /// section (folder + device rows, add-space), the global Active sessions
    /// list, the notice strip, and the UserMenu (§1.6).
    fn render_chat_sidebar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let user = self.state.read(cx).auth_user().cloned();

        // Keyed rows: (stable key, estimated height, element) — the key + height
        // list drives the §1.6 resort FLIP diff below (attention-bucket
        // promotions glide; cleared rows just go).
        let mut keyed: Vec<(String, f32, AnyElement)> = self.render_active_rows(theme, cx);
        // Imported memberships (globe rows) close out the same Sessions list —
        // no separate Shared section; the FLIP diff keys them like any row.
        keyed.extend(self.render_shared_rows(theme, cx));
        let settled_items: Vec<AnyElement> = self
            .render_settled_rows(theme, cx)
            .into_iter()
            .map(|(_, _, element)| element)
            .collect();

        // Resort glide (§1.6 View Transitions parity): when the ORDER of a live
        // list changes (new activity resort, grouping flip), surviving rows
        // glide from their old y to the new one — layout is already at the new
        // position; the offset is a paint-only relative inset animated to 0
        // over 260ms cubic-bezier(0.22,1,0.36,1). New rows fade in; removals
        // just go (matching the original). First fill and chat switches (which
        // don't reorder) never animate.
        let order: Vec<(String, f32)> = keyed.iter().map(|(k, h, _)| (k.clone(), *h)).collect();
        if self.sidebar_prev_order != order {
            if !self.sidebar_prev_order.is_empty() {
                let offsets = resort_offsets(&self.sidebar_prev_order, &order, SIDEBAR_LIST_GAP);
                let prev_keys: std::collections::HashSet<&str> = self
                    .sidebar_prev_order
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect();
                let new_keys: std::collections::HashSet<String> = order
                    .iter()
                    .filter(|(k, _)| !prev_keys.contains(k.as_str()))
                    .map(|(k, _)| k.clone())
                    .collect();
                if !offsets.is_empty() || !new_keys.is_empty() {
                    self.resort_epoch += 1;
                    self.sidebar_resort = offsets;
                    self.sidebar_new_keys = new_keys;
                }
            }
            self.sidebar_prev_order = order;
        }
        let epoch = self.resort_epoch;
        let list_items: Vec<AnyElement> = keyed
            .into_iter()
            .map(|(key, _, element)| {
                if let Some(dy) = self.sidebar_resort.get(&key).copied() {
                    let id = SharedString::from(format!("resort-{epoch}-{key}"));
                    div()
                        .child(element)
                        .with_animation(id, RESORT.animation(), move |el, t| {
                            el.relative().top(px(dy * (1.0 - t)))
                        })
                        .into_any_element()
                } else if self.sidebar_new_keys.contains(&key) {
                    let id = SharedString::from(format!("row-in-{epoch}-{key}"));
                    motion::fade_quick(id, div().child(element)).into_any_element()
                } else {
                    element
                }
            })
            .collect();

        // Overflow edge fades for the lists scroll region — the tab strip's
        // idiom, vertical (offset from the LAST frame; the lag is invisible).
        let (lists_fade_top, lists_fade_bottom) = self.sidebar_fade_zones();
        // Opaque platforms melt overflow into the surface tone with painted
        // gradient overlays. Over GLASS no overlay can work — the backdrop is
        // see-through blur, so tone stacks into a smudge and black reads as a
        // shadow (user reports). Instead the ROWS fade themselves: prepaint-
        // measured bounds drive per-row opacity toward the viewport edges
        // ([`Shell::sidebar_row_alpha`]), dissolving the edge to pure glass.
        let glass = theme.is_glass();
        let sidebar_fade = theme.surface;

        let user_line: SharedString = user
            .as_ref()
            .map(|u| u.name.clone().unwrap_or_else(|| u.email.clone()).into())
            .unwrap_or_else(|| SharedString::from("Not signed in"));
        let user_email: Option<SharedString> = user.as_ref().map(|u| u.email.clone().into());
        let user_menu = self.render_user_menu(user_line.clone(), user_email.clone(), theme, cx);

        let spaces_section = self.render_spaces_section(theme, cx);
        let (can_start_scaffold, starting_scaffold, scaffold_error) = {
            let state = self.state.read(cx);
            let starting = state.scaffold_session_creating()
                || state
                    .selected_chat
                    .as_deref()
                    .is_some_and(|chat_id| state.scaffold_chat_starting(chat_id));
            (
                state.can_start_scaffold_session(),
                starting,
                state.scaffold_session_error.clone(),
            )
        };
        let has_active_sessions = !list_items.is_empty();
        let has_settled_sessions = !settled_items.is_empty();
        let import_sessions_button = div()
            .id("import-local-session")
            .h(px(24.0))
            .px(px(7.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .rounded(px(6.0))
            .text_size(px(10.0))
            .text_color(theme.text_muted)
            .bg(motion::hover_blend(
                "import-local-session",
                crate::theme::wash(0.0),
                crate::theme::wash(0.12),
            ))
            .on_hover(motion::hover_listener("import-local-session"))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| this.open_session_import(cx)))
            .child(
                icon(icons::DOCUMENT_ADD)
                    .size(px(12.0))
                    .text_color(theme.text_muted),
            )
            .child(SharedString::from("Import"));
        let session_actions = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(import_sessions_button)
            .when(can_start_scaffold || starting_scaffold, |el| {
                el.child(
                    div()
                        .id("start-scaffold-session")
                        .h(px(24.0))
                        .px(px(7.0))
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .rounded(px(6.0))
                        .text_size(px(10.0))
                        .text_color(theme.text_muted)
                        .bg(motion::hover_blend(
                            "start-scaffold-session",
                            crate::theme::wash(0.0),
                            crate::theme::wash(0.12),
                        ))
                        .on_hover(motion::hover_listener("start-scaffold-session"))
                        .when(!starting_scaffold, |button| {
                            button
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.session_import_open = false;
                                    this.activity_open = false;
                                    this.invite_open = false;
                                    this.command_palette_open = false;
                                    this.state.update(cx, |state, cx| {
                                        state.start_scaffold_session(
                                            comet_proto::ScaffoldDatabaseEnvironment::Local,
                                            cx,
                                        );
                                    });
                                    cx.notify();
                                }))
                        })
                        .child(
                            icon(icons::GLOBAL)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from(if starting_scaffold {
                            "Starting…"
                        } else {
                            "Scaffold"
                        })),
                )
            });
        let sessions_header = div()
            .px(px(Theme::SPACE_SM))
            .pt(px(12.0))
            .pb(px(4.0))
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("Sessions")),
            )
            .child(session_actions);
        let settled_header = div()
            .px(px(Theme::SPACE_SM))
            .pt(px(12.0))
            .pb(px(4.0))
            .text_size(px(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.48))
            .child(SharedString::from("Settled"));

        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            // (No titlebar strip: the unified window titlebar spans the whole
            // window above this column.)
            // Spaces + the global Active list share one scroll region. On
            // glass the whole region paints inside an EdgeFade scope — a true
            // per-glyph gradient at active overflow edges.
            .child(crate::edge_fade::edge_faded(
                SIDEBAR_GLASS_FADE_BAND,
                glass && lists_fade_top,
                glass && lists_fade_bottom,
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("sidebar-lists")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.sidebar_scroll)
                            .px(px(Theme::SPACE_SM))
                            .flex()
                            .flex_col()
                            .child(spaces_section)
                            .child(sessions_header)
                            .when_some(scaffold_error, |el, error| {
                                el.child(
                                    div()
                                        .px(px(Theme::SPACE_SM))
                                        .pb(px(4.0))
                                        .text_size(px(11.0))
                                        .text_color(theme.danger_muted)
                                        .child(error),
                                )
                            })
                            .when(has_active_sessions, |el| {
                                el.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(SIDEBAR_LIST_GAP))
                                        .pb(px(Theme::SPACE_SM))
                                        .children(list_items),
                                )
                            })
                            .when(!has_active_sessions, |el| {
                                el.child(
                                    div()
                                        .px(px(Theme::SPACE_SM))
                                        .pb(px(Theme::SPACE_SM))
                                        .text_size(px(12.0))
                                        .text_color(theme.text_faint)
                                        .child(SharedString::from("No active sessions")),
                                )
                            })
                            .when(has_settled_sessions, |el| {
                                el.child(settled_header).child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(SIDEBAR_LIST_GAP))
                                        .pb(px(Theme::SPACE_SM))
                                        .children(settled_items),
                                )
                            }),
                    )
                    .when(lists_fade_top && !glass, |el| {
                        el.child(div().absolute().top_0().left_0().right_0().h(px(24.0)).bg(
                            gpui::linear_gradient(
                                180.0,
                                gpui::linear_color_stop(sidebar_fade, 0.0),
                                gpui::linear_color_stop(sidebar_fade.opacity(0.0), 1.0),
                            ),
                        ))
                    })
                    .when(lists_fade_bottom && !glass, |el| {
                        el.child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .h(px(24.0))
                                .bg(gpui::linear_gradient(
                                    0.0,
                                    gpui::linear_color_stop(sidebar_fade, 0.0),
                                    gpui::linear_color_stop(sidebar_fade.opacity(0.0), 1.0),
                                )),
                        )
                    }),
            ))
            // Update strip (above the user menu; below the lists).
            .when_some(self.render_update_strip(theme, cx), |el, strip| {
                el.child(strip)
            })
            // Inline mutation-failure notice.
            .when_some(self.sidebar_notice.clone(), |el, notice| {
                el.child(
                    div()
                        .id("sidebar-notice")
                        .mx(px(Theme::SPACE_SM))
                        .mb(px(Theme::SPACE_SM))
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.danger)
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_notice = None;
                            cx.notify();
                        }))
                        .child(notice),
                )
            })
            .child(div().p(px(Theme::SPACE_SM)).flex_none().child(user_menu))
            .into_any_element()
    }

    /// Update strip: shown above the user menu whenever the engine's
    /// UpdateStatus stream reports a newer release. On a macOS bundle install
    /// it drives the whole flow — click to download, then click to restart into
    /// the staged bundle. Elsewhere (managed/source installs) it is advisory
    /// (`comet update`); click dismisses it for that version.
    fn render_update_strip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.state.read(cx).update.clone()?;
        if !status.update_available {
            return None;
        }
        let latest = status.latest_version.clone()?;
        if self.update_dismissed.as_deref() == Some(latest.as_str()) {
            return None;
        }
        let mac_app = matches!(self.install, comet_update::InstallKind::MacApp { .. });

        let (label, clickable): (SharedString, bool) = if mac_app {
            match &self.update_flow {
                UpdateFlow::Idle => (format!("Update available — v{latest}").into(), true),
                UpdateFlow::Downloading => (format!("Downloading v{latest}…").into(), false),
                UpdateFlow::Ready(_) => ("Update ready — restart to apply".into(), true),
                UpdateFlow::Failed(message) => (format!("Update failed: {message}").into(), true),
            }
        } else {
            (
                format!("Update available — v{latest} · run `comet update`").into(),
                true,
            )
        };
        let failed = matches!(self.update_flow, UpdateFlow::Failed(_));
        let tone = if failed { theme.danger } else { theme.accent };
        // The chip fill is the sidebar's WHITE wash language, not an accent
        // tint: an indigo fill over the glass composited into a dark slab that
        // blocked the blur (user report) — the accent lives in the icon/text.
        let (chip_bg, chip_bg_hover) = if failed {
            (theme.danger.opacity(0.14), theme.danger.opacity(0.22))
        } else {
            (crate::theme::wash(0.11), crate::theme::wash(0.16))
        };

        let mut strip = div()
            .id("update-strip")
            .mx(px(Theme::SPACE_SM))
            // No bottom margin: the user-menu block below carries its own
            // SPACE_SM padding — doubling it read as a hole (user report).
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(chip_bg)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .text_size(px(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(tone)
            .child(
                icon(if failed {
                    icons::DANGER_TRIANGLE
                } else {
                    icons::RESTART
                })
                .size(px(14.0))
                .text_color(tone),
            )
            .child(div().flex_1().min_w_0().child(label));
        if clickable {
            strip = strip
                .cursor_pointer()
                .hover(move |s| s.bg(chip_bg_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.on_update_strip_click(cx)));
        }
        Some(strip.into_any_element())
    }

    /// Idle → download; Ready → swap + relaunch; Failed → retry; advisory
    /// installs → dismiss for this version.
    fn on_update_strip_click(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.install, comet_update::InstallKind::MacApp { .. }) {
            self.update_dismissed = self
                .state
                .read(cx)
                .update
                .as_ref()
                .and_then(|s| s.latest_version.clone());
            cx.notify();
            return;
        }
        match std::mem::replace(&mut self.update_flow, UpdateFlow::Idle) {
            UpdateFlow::Idle | UpdateFlow::Failed(_) => self.begin_update_download(cx),
            UpdateFlow::Downloading => self.update_flow = UpdateFlow::Downloading,
            UpdateFlow::Ready(staged) => self.apply_staged_update(staged, cx),
        }
    }

    /// Ask the local engine to fetch and verify the app bundle with the current
    /// short-lived Comet access token. The strip flips to "restart to apply"
    /// when staging completes.
    fn begin_update_download(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.update_flow = UpdateFlow::Failed("Update unavailable".into());
            cx.notify();
            return;
        };
        self.update_flow = UpdateFlow::Downloading;
        let download = Tokio::spawn(cx, async move {
            let value = engine
                .client()
                .call(methods::STAGE_UPDATE, serde_json::json!({}))
                .await
                .map_err(|error| error.to_string())?;
            value
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
                .ok_or_else(|| "Could not stage update".to_string())
        });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let outcome = match download.await {
                Ok(Ok(staged)) => Ok(staged),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.update_flow = match outcome {
                    Ok(staged) => UpdateFlow::Ready(staged),
                    Err(message) => {
                        tracing::warn!(%message, "update download failed");
                        UpdateFlow::Failed(message.into())
                    }
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Swap the staged bundle over the installed one, arm the detached
    /// relauncher, and quit — the relauncher `open`s the new bundle once this
    /// process (and its engine lock / IPC port) is gone.
    fn apply_staged_update(&mut self, staged: PathBuf, cx: &mut Context<Self>) {
        let comet_update::InstallKind::MacApp { bundle } = self.install.clone() else {
            return;
        };
        match comet_update::apply_mac_app(&staged, &bundle) {
            Ok(installed) => {
                comet_update::relaunch_app_after_exit(&installed);
                cx.quit();
            }
            Err(err) => {
                tracing::error!(error = %err, "update apply failed");
                self.update_flow = UpdateFlow::Failed(format!("{err:#}").into());
                cx.notify();
            }
        }
    }

    /// UserMenu (§1.6): name/email trigger row; menu with plan badge, Open
    /// settings, Sign out.
    fn render_user_menu(
        &mut self,
        user_line: SharedString,
        user_email: Option<SharedString>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.user_menu_open;
        // Bottom-of-sidebar identity (comet user-menu.tsx): avatar circle +
        // name with the plan label underneath, Alpha badge chip on the right.
        let initial: SharedString = user_line
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
            .into();
        let mut trigger = div()
            .id("user-menu")
            .flex_none()
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(Theme::SPACE_SM))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            // user-menu.tsx trigger: hover `bg-white/[0.04]`, open state
            // (`data-[state=open]`) the slightly stronger `bg-white/[0.06]`;
            // the hover wash fades over `transition-colors`.
            .bg(if open {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    "user-menu-trigger",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover().opacity(0.8),
                )
            })
            .on_hover(motion::hover_listener("user-menu-trigger"))
            .on_click(cx.listener(|this, _, _, cx| {
                // A click that just dismissed the menu (outside-click on the
                // trigger) must not instantly reopen it.
                let just_dismissed = this
                    .user_menu_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                this.user_menu_open = !this.user_menu_open && !just_dismissed;
                this.user_menu_dismissed_at = None;
                cx.notify();
            }))
            .child(
                // Avatar: white circle, initial in near-black (comet user-menu.tsx).
                div()
                    .size(px(28.0))
                    .flex_none()
                    .rounded_full()
                    .bg(theme.text)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.bg)
                    .child(initial),
            )
            .child(
                // Name with the plan label underneath — no chip on the right.
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .line_height(px(17.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .truncate()
                            .child(user_line.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .line_height(px(15.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Alpha")),
                    ),
            );
        if open {
            // user-menu.tsx content: `w-[--radix-dropdown-menu-trigger-width]`
            // (exactly as wide as the trigger row — sidebar minus its p-2
            // gutters), `flex-col gap-0.5`, then: one small muted email line
            // (`px-2 pb-1 pt-1.5 text-[11px] text-muted-foreground/70`),
            // "Settings", separator, "Sign out". Both rows are plain
            // `menuItem`s with muted 16px icons — sign-out carries NO
            // destructive tone in the original.
            let menu = popover::popover_card(theme)
                .w(px(self.settings.sidebar_width - 2.0 * Theme::SPACE_SM))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.user_menu_open = false;
                    this.user_menu_dismissed_at = Some(std::time::Instant::now());
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .px(px(8.0))
                        .pt(px(6.0))
                        .pb(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.7))
                        .truncate()
                        .child(user_email.unwrap_or(user_line)),
                )
                .child(
                    popover::menu_row(theme, false, "user-menu-settings")
                        .id("user-menu-settings")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_settings(SettingsSection::Devices, cx)
                        }))
                        .child(
                            icon(icons::SETTINGS_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Settings")),
                )
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(theme, false, "user-menu-signout")
                        .id("user-menu-signout")
                        .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                        .child(
                            icon(icons::LOGOUT_2)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Sign out")),
                )
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu_above("user-menu-popover", menu));
        }
        trigger.into_any_element()
    }

    fn render_activity_drawer(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let mut items = self
            .state
            .read(cx)
            .collaboration
            .as_ref()
            .map(crate::multiplayer::activity_items)
            .unwrap_or_default();
        let goal_groups = self.transcript_chrome.projection.goal_groups.clone();
        for feedback in &self.control_feedback {
            if items.iter().any(|item| item.id == feedback.command_id) {
                continue;
            }
            let (result, tone) = match feedback.state {
                ControlFeedbackState::Pending => {
                    ("pending", crate::multiplayer::ActivityTone::Pending)
                }
                ControlFeedbackState::Applied => {
                    ("applied", crate::multiplayer::ActivityTone::Success)
                }
                ControlFeedbackState::Rejected => {
                    ("rejected", crate::multiplayer::ActivityTone::Danger)
                }
            };
            items.push(crate::multiplayer::ActivityItem {
                id: feedback.command_id.clone(),
                actor: feedback.actor.to_string(),
                label: format!("{} {result}", feedback.action),
                detail: feedback.detail.as_ref().map(ToString::to_string),
                target_id: None,
                occurred_at: feedback.occurred_at,
                tone,
            });
        }
        items.sort_by_key(|item| std::cmp::Reverse(item.occurred_at));
        let now = Utc::now();
        let has_activity = !items.is_empty();
        let goal_total = goal_groups
            .iter()
            .map(|group| group.rows.len())
            .sum::<usize>();
        let goal_done = goal_groups
            .iter()
            .flat_map(|group| &group.rows)
            .filter(|goal| goal.done)
            .count();
        let goal_group_elements = goal_groups
            .into_iter()
            .enumerate()
            .map(|(index, group)| render_goal_group(group, index, "activity-goal", &theme))
            .collect::<Vec<_>>();
        let goals_card = (goal_total > 0).then(|| {
            div()
                .id("activity-goals")
                .mt(px(Theme::SPACE_LG))
                .mb(px(Theme::SPACE_MD))
                .p(px(Theme::SPACE_MD))
                .rounded(px(10.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface_raised)
                .child(
                    div()
                        .mb(px(Theme::SPACE_SM))
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(SharedString::from("Goals")),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(format!(
                                    "{goal_done}/{goal_total} done"
                                ))),
                        ),
                )
                .children(goal_group_elements)
                .into_any_element()
        });
        let rows = items.into_iter().map(|item| {
            let actor = {
                let state = self.state.read(cx);
                let resolved = state.participant_name(&item.actor);
                if resolved == "Teammate" {
                    item.actor.clone()
                } else {
                    resolved.to_string()
                }
            };
            let tone = match item.tone {
                crate::multiplayer::ActivityTone::Neutral => theme.text_faint,
                crate::multiplayer::ActivityTone::Pending => theme.warning,
                crate::multiplayer::ActivityTone::Success => theme.success,
                crate::multiplayer::ActivityTone::Danger => theme.danger,
            };
            let ago = chrono::DateTime::<Utc>::from_timestamp_millis(item.occurred_at)
                .map(|at| format_time_ago(at, now))
                .unwrap_or_default();
            let target_id = item.target_id.clone();
            let row_id = item.id.clone();
            div()
                .id(SharedString::from(format!("activity-{row_id}")))
                .py(px(Theme::SPACE_SM))
                .border_b_1()
                .border_color(theme.border)
                .flex()
                .gap(px(Theme::SPACE_SM))
                .when_some(target_id, |el, target_id| {
                    el.cursor_pointer()
                        .hover(|style| style.bg(theme.surface_raised_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_annotation_target(target_id.clone(), cx);
                        }))
                })
                .child(
                    div()
                        .mt(px(Theme::SPACE_XS))
                        .size(px(6.0))
                        .rounded_full()
                        .bg(tone),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(Theme::SPACE_XS))
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .child(SharedString::from(item.label)),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(theme.text_faint)
                                        .child(SharedString::from(ago)),
                                ),
                        )
                        .child(
                            div()
                                .mt(px(2.0))
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(actor)),
                        )
                        .when_some(item.detail.map(SharedString::from), |el, detail| {
                            el.child(
                                div()
                                    .mt(px(Theme::SPACE_XS))
                                    .text_size(px(11.0))
                                    .text_color(theme.text_faint)
                                    .child(detail),
                            )
                        }),
                )
        });
        let width = if f32::from(viewport.width) < RIGHT_PANE_MIN * 2.0 {
            viewport.width
        } else {
            px(RIGHT_PANE_MIN)
        };
        let drawer = div()
            .id("activity-drawer")
            .w(width)
            .h(viewport.height)
            .bg(theme.surface_dialog)
            .border_l_1()
            .border_color(theme.border)
            .shadow_lg()
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.activity_open = false;
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.activity_open = false;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .h(px(48.0))
                    .flex_none()
                    .px(px(Theme::SPACE_LG))
                    .border_b_1()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(14.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(SharedString::from("Activity")),
                    )
                    .child(
                        popover::btn_ghost(&theme, "Close", "close-activity")
                            .id("close-activity")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.activity_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("activity-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(Theme::SPACE_LG))
                    .when_some(goals_card, |el, card| el.child(card))
                    .when(!has_activity && goal_total == 0, |el| {
                        el.child(
                            div()
                                .py(px(Theme::SPACE_LG))
                                .text_size(px(12.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(
                                    "Agent goals and shared activity appear here.",
                                )),
                        )
                    })
                    .children(rows),
            );
        right_drawer_overlay(viewport, drawer)
    }

    fn render_invite_dialog(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let chat_id = self.state.read(cx).selected_chat.clone()?;
        let link = self.session_link_for(&chat_id, cx);
        let theme = Theme::of(cx).clone();
        let section_label = |text: &'static str| {
            div()
                .mt(px(Theme::SPACE_MD))
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text_faint)
                .child(SharedString::from(text))
        };
        let value_box = |value: SharedString| {
            div()
                .mt(px(Theme::SPACE_XS))
                .px(px(Theme::SPACE_SM))
                .py(px(Theme::SPACE_SM))
                .rounded(px(Theme::CONTROL_RADIUS))
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg)
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .overflow_hidden()
                .child(value)
        };
        let card = popover::dialog_card(&theme)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.invite_open = false;
                    cx.notify();
                }
            }))
            .child(popover::dialog_title(&theme, "Invite teammate"))
            .child(div().mt(px(Theme::SPACE_SM)).child(popover::dialog_body(
                &theme,
                "Share the link for one-click join, or the session ID other \
                 sessions use to reach this one (comet session …).",
            )))
            .child(section_label("Session ID"))
            .child(value_box(SharedString::from(chat_id.clone())))
            .child(section_label("Invite link"))
            .child(match link.clone() {
                Some(link) => value_box(SharedString::from(link)).into_any_element(),
                None => div()
                    .mt(px(Theme::SPACE_XS))
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(
                        "No shareable grant for this session yet — the link \
                         appears once the session is joinable.",
                    ))
                    .into_any_element(),
            })
            .when_some(self.link_feedback.clone(), |el, feedback| {
                el.child(
                    div()
                        .mt(px(Theme::SPACE_SM))
                        .text_size(px(11.0))
                        .text_color(theme.success)
                        .child(feedback),
                )
            })
            .child(
                div()
                    .mt(px(Theme::SPACE_LG))
                    .flex()
                    .justify_end()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "close-invite")
                            .id("close-invite")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.invite_open = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_ghost(&theme, "Copy ID", "copy-session-id")
                            .id("copy-session-id")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.copy_selected_session_id(cx)),
                            ),
                    )
                    .when(link.is_some(), |row| {
                        row.child(
                            popover::btn_ghost(&theme, "Open", "open-session-link")
                                .id("open-session-link")
                                .on_click(cx.listener(|this, _, _, cx| this.open_session_link(cx))),
                        )
                        .child(
                            popover::btn_primary(&theme, "Copy link")
                                .id("copy-session-link")
                                .on_click(cx.listener(|this, _, _, cx| this.copy_session_link(cx))),
                        )
                    }),
            )
            .into_any_element();
        Some(popover::modal("invite-dialog", viewport, card))
    }

    fn render_command_palette(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let row = |label: &'static str, hint: &'static str, id: &'static str| {
            popover::menu_row(&theme, false, id)
                .id(id)
                .child(div().flex_1().child(SharedString::from(label)))
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(hint)),
                )
        };
        let card = popover::popover_card(&theme)
            .w(px(RIGHT_PANE_MIN))
            .p(px(Theme::SPACE_SM))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => this.command_palette_open = false,
                    "n" => {
                        this.command_palette_open = false;
                        this.composer
                            .update(cx, |composer, cx| composer.begin_start_agent(window, cx));
                    }
                    "a" => {
                        this.command_palette_open = false;
                        this.activity_open = true;
                    }
                    "i" => {
                        this.command_palette_open = false;
                        this.invite_open = true;
                    }
                    "f" => {
                        this.command_palette_open = false;
                        this.toggle_focus_mode(cx);
                    }
                    _ => return,
                }
                cx.notify();
            }))
            .child(
                div()
                    .px(px(Theme::SPACE_SM))
                    .py(px(Theme::SPACE_SM))
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(SharedString::from("Commands")),
            )
            .child(
                row("Start agent", "N", "command-start-agent").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.command_palette_open = false;
                        this.composer
                            .update(cx, |composer, cx| composer.begin_start_agent(window, cx));
                    },
                )),
            )
            .child(
                row("Open activity", "A", "command-activity").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.command_palette_open = false;
                        this.activity_open = true;
                        cx.notify();
                    },
                )),
            )
            .child(
                row("Invite teammate", "I", "command-invite").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.command_palette_open = false;
                        this.invite_open = true;
                        cx.notify();
                    },
                )),
            )
            .child(
                row("Toggle focus", "F", "command-focus").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.command_palette_open = false;
                        this.toggle_focus_mode(cx);
                    },
                )),
            )
            .into_any_element();
        popover::modal("command-palette", viewport, card)
    }

    fn render_selection_annotation_popover(
        &mut self,
        viewport: gpui::Size<Pixels>,
        popup_origin: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let inspector = self.annotation_inspector.as_ref()?;
        let selected = inspector.annotation.clone();
        let input = inspector.input.clone();
        let is_new = inspector.is_new;
        let error = inspector.error.clone();
        let can_annotate = self
            .state
            .read(cx)
            .has_collaboration_capability(comet_proto::CAPABILITY_SESSION_ANNOTATE);
        let exact = selected
            .anchor
            .exact
            .as_deref()
            .map(str::trim)
            .filter(|exact| !exact.is_empty())
            .map(SharedString::from);
        let prompt_annotation = selected.clone();
        let width = px((f32::from(viewport.width) - 24.0).clamp(240.0, 340.0));

        let card = popover::popover_card(&theme)
            .id("selection-comment-popover")
            .w(width)
            .p(px(0.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.annotation_inspector = None;
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.annotation_inspector = None;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .h(px(40.0))
                    .px(px(Theme::SPACE_MD))
                    .border_b_1()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(SharedString::from("Comment")),
            )
            .when_some(exact, |card, exact| {
                card.child(
                    div()
                        .mx(px(Theme::SPACE_MD))
                        .mt(px(Theme::SPACE_MD))
                        .max_h(px(58.0))
                        .overflow_hidden()
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface_raised)
                        .px(px(Theme::SPACE_SM))
                        .py(px(Theme::SPACE_XS))
                        .text_size(px(11.0))
                        .line_height(px(16.0))
                        .text_color(theme.text_muted)
                        .child(exact),
                )
            })
            .child(
                div()
                    .px(px(Theme::SPACE_MD))
                    .pt(px(Theme::SPACE_MD))
                    .when(can_annotate, |body| {
                        body.child(
                            popover::dialog_field(input.into_any_element())
                                .min_h(px(72.0))
                                .items_start(),
                        )
                    })
                    .when(!can_annotate, |body| {
                        body.child(
                            div()
                                .text_size(px(12.0))
                                .line_height(px(18.0))
                                .text_color(theme.text)
                                .child(SharedString::from(selected.body.clone())),
                        )
                    })
                    .when_some(error, |body, error| {
                        body.child(
                            div()
                                .mt(px(Theme::SPACE_XS))
                                .text_size(px(11.0))
                                .text_color(theme.danger)
                                .child(error),
                        )
                    }),
            )
            .child(
                div()
                    .px(px(Theme::SPACE_MD))
                    .py(px(Theme::SPACE_MD))
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(Theme::SPACE_SM))
                    .when(!is_new, |actions| {
                        actions.child(
                            popover::btn_ghost(&theme, "Add to prompt", "selection-comment-prompt")
                                .id("selection-comment-prompt")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.append_annotation_to_prompt(
                                        prompt_annotation.clone(),
                                        window,
                                        cx,
                                    )
                                })),
                        )
                    })
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "selection-comment-cancel")
                            .id("selection-comment-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.annotation_inspector = None;
                                cx.notify();
                            })),
                    )
                    .when(can_annotate, |actions| {
                        actions.child(
                            popover::btn_primary(&theme, if is_new { "Comment" } else { "Save" })
                                .id("annotation-save")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.save_annotation(window, cx)
                                })),
                        )
                    }),
            )
            .into_any_element();

        Some(popover::menu_at(
            "selection-comment-popover-layer",
            popup_origin,
            card,
        ))
    }

    fn render_annotation_inspector(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let Some(popup_origin) = self
            .annotation_inspector
            .as_ref()
            .and_then(|inspector| inspector.popup_origin)
        {
            return self.render_selection_annotation_popover(viewport, popup_origin, cx);
        }
        let theme = Theme::of(cx).clone();
        let inspector = self.annotation_inspector.as_ref()?;
        let selected = inspector.annotation.clone();
        let is_new = inspector.is_new;
        let target_id = selected.anchor.target_id.clone();
        let input = inspector.input.clone();
        let error = inspector.error.clone();
        let can_annotate = self
            .state
            .read(cx)
            .has_collaboration_capability(comet_proto::CAPABILITY_SESSION_ANNOTATE);
        let actor_name = self
            .state
            .read(cx)
            .participant_name(&selected.author_subject)
            .to_string();
        let source =
            (selected.anchor.target_kind == comet_proto::AnchorTargetKind::File).then(|| {
                format!(
                    "{} · {}",
                    crate::multiplayer::file_target_source_label(
                        crate::multiplayer::file_target_source(selected.anchor.file.as_ref()),
                    ),
                    crate::multiplayer::file_target_label(selected.anchor.file.as_ref()),
                )
            });
        let range = selected
            .anchor
            .byte_range
            .as_ref()
            .map(|range| format!("Bytes {}–{}", range.start, range.end));
        let annotations = {
            let state = self.state.read(cx);
            let mut seen = std::collections::HashSet::new();
            state
                .collaboration
                .as_ref()
                .map(|snapshot| {
                    snapshot
                        .publications
                        .iter()
                        .rev()
                        .filter_map(|publication| {
                            let comet_proto::PublicationValue::Annotation(annotation) =
                                &publication.value
                            else {
                                return None;
                            };
                            (annotation.anchor.target_id == target_id
                                && seen.insert(annotation.id.clone()))
                            .then(|| annotation.clone())
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let width = if f32::from(viewport.width) < RIGHT_PANE_MIN * 2.0 {
            viewport.width
        } else {
            px(RIGHT_PANE_MIN)
        };
        let rows = annotations.into_iter().map(|annotation| {
            let author = self
                .state
                .read(cx)
                .participant_name(&annotation.author_subject)
                .to_string();
            let resolved = annotation.resolved_at.is_some();
            let prompt_annotation = annotation.clone();
            let prompt_action_id: SharedString =
                format!("annotation-prompt-{}", annotation.id).into();
            div()
                .px(px(Theme::SPACE_LG))
                .py(px(Theme::SPACE_MD))
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(Theme::SPACE_SM))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text_muted)
                                .child(SharedString::from(author)),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(if resolved {
                                    theme.success
                                } else {
                                    theme.text_faint
                                })
                                .child(SharedString::from(if resolved {
                                    "Resolved"
                                } else {
                                    crate::multiplayer::annotation_state_label(annotation.state)
                                })),
                        ),
                )
                .child(
                    div()
                        .mt(px(Theme::SPACE_XS))
                        .text_size(px(12.0))
                        .line_height(px(18.0))
                        .text_color(theme.text)
                        .child(SharedString::from(annotation.body)),
                )
                .child(
                    div().mt(px(Theme::SPACE_SM)).child(
                        popover::btn_ghost(&theme, "Add to prompt", prompt_action_id.clone())
                            .id(prompt_action_id)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.append_annotation_to_prompt(
                                    prompt_annotation.clone(),
                                    window,
                                    cx,
                                )
                            })),
                    ),
                )
        });
        let drawer = div()
            .id("annotation-inspector")
            .w(width)
            .h(viewport.height)
            .bg(theme.surface_dialog)
            .border_l_1()
            .border_color(theme.border)
            .shadow_lg()
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.annotation_inspector = None;
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.annotation_inspector = None;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .h(px(48.0))
                    .flex_none()
                    .px(px(Theme::SPACE_LG))
                    .border_b_1()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(SharedString::from("Notes")),
                    )
                    .child(
                        popover::btn_ghost(&theme, "Close", "close-annotations")
                            .id("close-annotations")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.annotation_inspector = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("annotation-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .px(px(Theme::SPACE_LG))
                            .py(px(Theme::SPACE_MD))
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(format!(
                                        "{} · {}",
                                        actor_name,
                                        crate::multiplayer::annotation_state_label(selected.state,)
                                    ))),
                            )
                            .when_some(source.map(SharedString::from), |el, source| {
                                el.child(
                                    div()
                                        .mt(px(Theme::SPACE_XS))
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted)
                                        .child(source),
                                )
                            })
                            .when_some(range.map(SharedString::from), |el, range| {
                                el.child(
                                    div()
                                        .mt(px(2.0))
                                        .text_size(px(10.0))
                                        .text_color(theme.text_faint)
                                        .child(range),
                                )
                            }),
                    )
                    .children(rows),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(Theme::SPACE_LG))
                    .py(px(Theme::SPACE_MD))
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .mb(px(Theme::SPACE_SM))
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(SharedString::from(if !can_annotate {
                                "View only"
                            } else if is_new {
                                "Add comment"
                            } else {
                                "Edit comment"
                            })),
                    )
                    .when(can_annotate, |el| el.child(input))
                    .when_some(error, |el, error| {
                        el.child(
                            div()
                                .mt(px(Theme::SPACE_XS))
                                .text_size(px(11.0))
                                .text_color(theme.danger)
                                .child(error),
                        )
                    })
                    .when(can_annotate, |el| {
                        el.child(
                            div()
                                .mt(px(Theme::SPACE_SM))
                                .flex()
                                .items_center()
                                .gap(px(Theme::SPACE_SM))
                                .child(
                                    popover::btn_primary(&theme, "Save")
                                        .id("annotation-save")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.save_annotation(window, cx)
                                        })),
                                )
                                .when(!is_new, |actions| {
                                    actions.child(
                                        popover::btn_ghost(
                                            &theme,
                                            if selected.resolved_at.is_some() {
                                                "Reopen"
                                            } else {
                                                "Resolve"
                                            },
                                            "resolve-annotation",
                                        )
                                        .id("annotation-resolve")
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.set_annotation_resolved(
                                                    selected.resolved_at.is_none(),
                                                    cx,
                                                )
                                            }),
                                        ),
                                    )
                                }),
                        )
                    }),
            )
            .into_any_element();
        Some(right_drawer_overlay(viewport, drawer))
    }

    fn render_selection_comment_action(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !matches!(self.route, Route::Chat)
            || self.annotation_inspector.is_some()
            || !self
                .state
                .read(cx)
                .has_collaboration_capability(comet_proto::CAPABILITY_SESSION_ANNOTATE)
        {
            return None;
        }
        let (message_id, exact, (pointer_x, pointer_y)) =
            crate::markdown::selection::selected_message_context()?;
        if exact.trim().is_empty() {
            return None;
        }
        let theme = Theme::of(cx);
        let left = pointer_x.clamp(12.0, (f32::from(viewport.width) - 116.0).max(12.0));
        let top = (pointer_y + 10.0).clamp(12.0, (f32::from(viewport.height) - 42.0).max(12.0));
        let action = div()
            .id("selection-comment")
            .h(px(32.0))
            .px(px(12.0))
            .rounded_full()
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_raised)
            .shadow_md()
            .flex()
            .items_center()
            .gap(px(6.0))
            .occlude()
            .cursor_pointer()
            .hover(|style| style.bg(theme.surface_raised_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.open_selection_annotation(
                        message_id.clone(),
                        exact.clone(),
                        gpui::point(px(left), px(top + 38.0)),
                        cx,
                    )
                }),
            )
            .child(
                icon(icons::CHAT_ROUND_LINE)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from("Comment")),
            )
            .into_any_element();
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(gpui::point(px(left), px(top)))
                    .anchor(gpui::Anchor::TopLeft)
                    .child(action),
            )
            .priority(1)
            .into_any_element(),
        )
    }

    /// Floating layers owned by the shell: the session context menu and the
    /// rename / delete-confirm dialogs.
    fn render_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();
        if let Some(action) = self.render_selection_comment_action(viewport, cx) {
            overlays.push(action);
        }

        if let Some((chat_id, position)) = self.chat_menu.clone() {
            let rename_id = chat_id.clone();
            let archive_id = chat_id.clone();
            let delete_id = chat_id.clone();
            let unread_id = chat_id.clone();
            let copy_id = chat_id.clone();
            let link_id = chat_id.clone();
            let invite_id = chat_id.clone();
            let handoff_id = chat_id.clone();
            let has_invite_link = self.session_link_for(&chat_id, cx).is_some();
            let (is_settled, unread_toggle, can_handoff) = {
                let state = self.state.read(cx);
                let chat = state.chats.iter().find(|chat| chat.id == chat_id);
                (
                    chat.is_some_and(|chat| chat.archived),
                    // Unread only exists relative to activity: rows without a
                    // message yet offer neither direction of the toggle.
                    chat.filter(|chat| chat.last_message_at.is_some())
                        .map(|chat| chat.unseen()),
                    state.chat_can_handoff_to_scaffold(&chat_id),
                )
            };
            let menu = popover::popover_card(&theme)
                .w(px(210.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.chat_menu = None;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-rename-{chat_id}"))
                        .id("chat-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_chat(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                )
                .when(!is_settled, |menu| {
                    menu.child(
                        popover::menu_row(&theme, false, format!("chat-menu-archive-{chat_id}"))
                            .id("chat-menu-archive")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_chat_settled(archive_id.clone(), true, cx)
                            }))
                            .child(
                                icon(icons::ARCHIVE_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Settle")),
                    )
                })
                .when_some(unread_toggle, |menu, unseen| {
                    menu.child(
                        popover::menu_row(&theme, false, format!("chat-menu-unread-{chat_id}"))
                            .id("chat-menu-unread")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.chat_menu = None;
                                this.state.update(cx, |state, cx| {
                                    if unseen {
                                        state.mark_chat_seen(&unread_id, cx);
                                    } else {
                                        state.mark_chat_unread(&unread_id, cx);
                                    }
                                });
                                cx.notify();
                            }))
                            // The dot the action raises (or clears) is its own
                            // best icon — accent-filled for "mark as unread",
                            // hollow for "mark as read".
                            .child(
                                div()
                                    .size(px(16.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(div().size(px(7.0)).rounded_full().map(|dot| {
                                        if unseen {
                                            dot.border_1().border_color(theme.text_muted)
                                        } else {
                                            dot.bg(theme.attention)
                                        }
                                    })),
                            )
                            .child(SharedString::from(if unseen {
                                "Mark as read"
                            } else {
                                "Mark as unread"
                            })),
                    )
                })
                .child(popover::menu_separator())
                .when(can_handoff, |menu| {
                    menu.child(
                        popover::menu_row(&theme, false, format!("chat-menu-handoff-{chat_id}"))
                            .id("chat-menu-handoff")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.chat_menu = None;
                                this.state.update(cx, |state, cx| {
                                    state.handoff_session_to_scaffold(
                                        handoff_id.clone(),
                                        comet_proto::ScaffoldDatabaseEnvironment::Local,
                                        cx,
                                    );
                                });
                                cx.notify();
                            }))
                            .child(
                                icon(icons::GLOBAL)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Hand off to Scaffold")),
                    )
                })
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-copy-id-{chat_id}"))
                        .id("chat-menu-copy-id")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy_chat_session_id(&copy_id, cx);
                        }))
                        .child(
                            icon(icons::COPY)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Copy session ID")),
                )
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-invite-{chat_id}"))
                        .id("chat-menu-invite")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_invite_for(invite_id.clone(), cx);
                        }))
                        .child(
                            icon(icons::ADD_CIRCLE)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Invite…")),
                )
                .when(has_invite_link, |menu| {
                    menu.child(
                        popover::menu_row(&theme, false, format!("chat-menu-copy-link-{chat_id}"))
                            .id("chat-menu-copy-link")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy_chat_invite_link(&link_id, cx);
                            }))
                            .child(
                                icon(icons::GLOBAL)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Copy invite link")),
                    )
                })
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-delete-{chat_id}"))
                        .id("chat-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.chat_menu = None;
                            this.delete_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Delete…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at("chat-context-menu", position, menu));
        }

        if let Some(dialog) = &mut self.rename_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename session"))
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
                            popover::btn_ghost(&theme, "Cancel", "rename-chat-cancel")
                                .id("rename-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-chat-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_chat(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-chat-dialog", viewport, card));
        }

        overlays.extend(self.render_space_overlays(viewport, window, cx));
        if let Some(overlay) = self.render_add_space_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }

        if self.activity_open {
            overlays.push(self.render_activity_drawer(viewport, cx));
        }
        if self.invite_open
            && let Some(invite) = self.render_invite_dialog(viewport, cx)
        {
            overlays.push(invite);
        }
        if self.command_palette_open {
            overlays.push(self.render_command_palette(viewport, cx));
        }
        if self.session_import_open {
            overlays.push(self.render_session_import_overlay(viewport, cx));
        }
        if let Some(inspector) = self.render_annotation_inspector(viewport, cx) {
            overlays.push(inspector);
        }

        if let Some(chat_id) = self.delete_confirm.clone() {
            let title = transcript::single_line(
                &self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_else(|| "New session".into()),
            );
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Delete session?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("\u{201C}{title}\u{201D} will be permanently deleted. This can\u{2019}t be undone."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-chat-cancel")
                                .id("delete-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Delete")
                                .id("delete-chat-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_chat(chat_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-chat-dialog", viewport, card));
        }

        overlays
    }

    fn resize_handle<T>(
        &self,
        id: &'static str,
        marker: fn() -> T,
        reset: fn(&mut Shell, &mut Context<Shell>),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div>
    where
        T: 'static,
    {
        let hover = Theme::of(cx).border_strong;
        div()
            .id(id)
            .w(px(5.0))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .hover(move |s| s.bg(hover))
            .on_drag(marker(), |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        reset(this, cx);
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            )
    }

    /// Floating presence cluster over the transcript's top-left corner —
    /// the barless remnant of the old session toolbar. REMOTE participants
    /// only: your own avatar is noise, so nothing renders while you're alone
    /// in the room (user request).
    fn render_remote_presence(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let remote = {
            let state = self.state.read(cx);
            state.selected_chat.as_ref()?;
            crate::multiplayer::remote_participants(state.participants(), state.principal_subject())
        };
        if remote.is_empty() {
            return None;
        }
        let avatars = remote.iter().enumerate().map(|(ix, participant)| {
            let name: SharedString = participant
                .display_name
                .clone()
                .unwrap_or_else(|| participant.principal_subject.clone())
                .into();
            let initials = crate::multiplayer::initials(name.as_ref());
            let presence = match participant.state {
                comet_proto::ParticipantState::Active => theme.success,
                comet_proto::ParticipantState::Idle => theme.warning,
                comet_proto::ParticipantState::Disconnected => theme.text_faint,
            };
            div()
                .relative()
                .when(ix > 0, |el| el.ml(px(-6.0)))
                .size(px(24.0))
                .rounded_full()
                .border_1()
                .border_color(theme.bg)
                .bg(theme.surface_raised)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(9.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text_muted)
                .child(SharedString::from(initials))
                .child(
                    div()
                        .absolute()
                        .right(px(-1.0))
                        .bottom(px(-1.0))
                        .size(px(6.0))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.bg)
                        .bg(presence),
                )
        });
        Some(
            div()
                .absolute()
                .top(px(Theme::SPACE_SM))
                .left(px(Theme::SPACE_MD))
                .flex()
                .items_center()
                .children(avatars)
                .into_any_element(),
        )
    }

    fn render_workspace_status(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let preferred_harness = {
            let state = self.state.read(cx);
            let session_harness = state.selected_chat.as_deref().and_then(|chat_id| {
                let selected_id = state.selected_agent_session.as_deref()?;
                state
                    .collaboration_sessions(chat_id)
                    .find(|session| session.session_id == selected_id)
                    .and_then(|session| session.harness)
            });
            session_harness.or_else(|| {
                state
                    .selected_chat_row()
                    .and_then(|chat| chat.config.as_ref().map(|config| config.harness))
            })
        };
        let changes = self.changes_pane(cx);
        changes.update(cx, |changes, cx| changes.ensure_watch(cx));
        let changes_summary = changes.read(cx).summary(cx);
        let git_detected = self.space_git_detected(cx);
        let (branch, worktree_path) = {
            let state = self.state.read(cx);
            let chat = state.selected_chat_row();
            let branch: SharedString = chat
                .and_then(|chat| chat.branch.clone())
                .filter(|branch| !branch.is_empty())
                .unwrap_or_else(|| "Worktree".to_string())
                .into();
            let path = chat
                .and_then(|chat| chat.cwd.clone())
                .or_else(|| state.selected_space_row().map(|space| space.path.clone()));
            (branch, path)
        };
        let worktree_copied = worktree_path
            .as_deref()
            .is_some_and(|path| self.copied_worktree.as_deref() == Some(path));
        let goal_groups = self.transcript_chrome.projection.goal_groups.clone();
        let active_goal = self.transcript_chrome.projection.active_goal.clone();
        let goal_total = goal_groups
            .iter()
            .map(|group| group.rows.len())
            .sum::<usize>();
        let goal_done = goal_groups
            .iter()
            .flat_map(|group| &group.rows)
            .filter(|goal| goal.done)
            .count();
        let goal_group_elements = goal_groups
            .into_iter()
            .enumerate()
            .map(|(index, group)| render_goal_group(group, index, "workspace-goal", &theme))
            .collect::<Vec<_>>();

        let account = self
            .active_account_id
            .as_ref()
            .and_then(|active_account_id| {
                self.account_usage.as_ref().and_then(|snapshot| {
                    snapshot
                        .accounts
                        .iter()
                        .find(|account| {
                            !account.migration_available && account.id == *active_account_id
                        })
                        .map(|account| {
                            let provider = crate::multiplayer::harness_label(account.harness);
                            let identity = account
                                .display_name
                                .as_deref()
                                .or(account.email.as_deref())
                                .unwrap_or(provider);
                            let identity = account
                                .plan_label
                                .as_deref()
                                .map(|plan| format!("{identity} · {plan}"))
                                .unwrap_or_else(|| identity.to_string());
                            (account.harness, identity, account.usage_windows.clone())
                        })
                })
            });
        let (account_harness, account_identity, usage_windows) = account.unwrap_or_else(|| {
            (
                preferred_harness.unwrap_or(comet_proto::HarnessId::Codex),
                self.active_account_id.clone().unwrap_or_default(),
                Vec::new(),
            )
        });
        let account_icon = match account_harness {
            comet_proto::HarnessId::Codex => icons::OPENAI_MARK,
            comet_proto::HarnessId::ClaudeCode => icons::CLAUDE_MARK,
            comet_proto::HarnessId::Cursor => icons::CURSOR_MARK,
            _ => icons::CREW_MARK,
        };
        let account_icon_color = if account_harness == comet_proto::HarnessId::ClaudeCode {
            icons::claude_brand()
        } else {
            theme.text_muted
        };
        let changes_detail = match changes_summary {
            Some(summary) if summary.file_count == 0 => "Working tree clean".to_string(),
            Some(summary) if summary.file_count == 1 => "1 changed file".to_string(),
            Some(summary) => format!("{} changed files", summary.file_count),
            None if git_detected => "Checking the current worktree…".to_string(),
            None => "No Git worktree detected".to_string(),
        };
        let changes_stats = changes_summary.map(|summary| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(px(11.5))
                .child(
                    div()
                        .text_color(theme.diff_add)
                        .child(SharedString::from(format!("+{}", summary.additions))),
                )
                .child(
                    div()
                        .text_color(theme.diff_del)
                        .child(SharedString::from(format!("−{}", summary.deletions))),
                )
                .into_any_element()
        });
        let usage_meter_rows = usage_windows
            .iter()
            .take(2)
            .enumerate()
            .map(|(index, window)| {
                let fraction = window.used_fraction.clamp(0.0, 1.0);
                let fill = usage_color(usage_level(fraction), &theme);
                let reset = format_reset(window.resets_at, Utc::now())
                    .map(|reset| format!(" · {reset}"))
                    .unwrap_or_default();
                div()
                    .id(("workspace-usage-window", index))
                    .mt(px(7.0))
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(format!("{}{reset}", window.label)))
                            .child(div().flex_1())
                            .child(SharedString::from(format!(
                                "{}% used",
                                (fraction * 100.0).round() as u32
                            ))),
                    )
                    .child(
                        div()
                            .h(px(5.0))
                            .w_full()
                            .rounded_full()
                            .overflow_hidden()
                            .bg(crate::theme::ink(0.07))
                            .when(fraction > 0.0, |el| {
                                el.child(
                                    div()
                                        .h_full()
                                        .w(gpui::relative(fraction.max(0.015)))
                                        .rounded_full()
                                        .bg(fill),
                                )
                            }),
                    )
            })
            .collect::<Vec<_>>();
        let account_detail: SharedString = if !account_identity.is_empty() {
            account_identity.into()
        } else if self.active_account_loading || self.account_usage_loading {
            "Resolving current account…".into()
        } else if self.account_usage_error.is_some() {
            "Usage unavailable".into()
        } else {
            "No routed agent account yet".into()
        };

        let changes_row = div()
            .id("workspace-status-changes")
            .px(px(14.0))
            .py(px(11.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            .hover(|style| style.bg(theme.element_hover))
            .on_click(cx.listener(|this, _, _, cx| this.open_workspace_changes(cx)))
            .child(
                icon(icons::DOCUMENT)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child("Changes"),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(changes_detail)),
                    ),
            )
            .when_some(changes_stats, |el, stats| el.child(stats));
        let account_row = div()
            .id("workspace-status-account")
            .px(px(14.0))
            .py(px(11.0))
            .cursor_pointer()
            .hover(|style| style.bg(theme.element_hover))
            .on_click(cx.listener(|this, _, _, cx| {
                this.open_settings(SettingsSection::Agents, cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        icon(account_icon)
                            .size(px(16.0))
                            .text_color(account_icon_color),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child("Account usage"),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_faint)
                                    .child(account_detail),
                            ),
                    ),
            )
            .children(usage_meter_rows);
        let has_active_goal = active_goal.is_some();
        let active_goal_element = active_goal.map(|goal| {
            let status: SharedString = goal.status.replace('-', " ").into();
            div()
                .id("workspace-active-omp-goal")
                .mb(px(8.0))
                .p(px(9.0))
                .rounded(px(9.0))
                .bg(theme.element_hover)
                .flex()
                .items_start()
                .gap(px(8.0))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.accent)
                                        .child("OMP goal"),
                                )
                                .child(
                                    div()
                                        .text_size(px(9.5))
                                        .text_color(theme.text_faint)
                                        .child(status),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.text)
                                .child(SharedString::from(goal.objective)),
                        ),
                )
                .child(
                    div()
                        .id("workspace-drop-omp-goal")
                        .size(px(24.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .hover(|el| el.bg(theme.glass_hover()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.drop_active_omp_goal(cx);
                        }))
                        .child(
                            icon(icons::CLOSE)
                                .size(px(11.0))
                                .text_color(theme.text_faint),
                        ),
                )
        });

        let goals_section = div()
            .id("workspace-status-goals")
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(42.0))
                    .flex_none()
                    .px(px(14.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        icon(icons::CHECKLIST)
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child("Goals"),
                    )
                    .child(div().flex_1())
                    .when(goal_total > 0, |el| {
                        el.child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(format!("{goal_done} / {goal_total}"))),
                        )
                    }),
            )
            .child(
                div()
                    .id("workspace-goals-scroll")
                    .max_h(px(WORKSPACE_GOALS_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .track_scroll(&self.workspace_goals_scroll)
                    .px(px(14.0))
                    .pb(px(10.0))
                    .children(active_goal_element)
                    .when(goal_total == 0 && !has_active_goal, |el| {
                        el.child(
                            div()
                                .pb(px(4.0))
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child("No goals published yet"),
                        )
                    })
                    .children(goal_group_elements),
            );

        popover::popover_card(&theme)
            .id("workspace-status-card")
            .w_full()
            .p(px(0.0))
            .rounded(px(16.0))
            .overflow_hidden()
            .child(
                div()
                    .id("workspace-copy-worktree")
                    .px(px(14.0))
                    .pt(px(12.0))
                    .pb(px(10.0))
                    .flex()
                    .items_center()
                    .when(worktree_path.is_some(), |el| {
                        el.cursor_pointer()
                            .hover(|style| style.bg(theme.element_hover))
                            .tooltip(popover::text_tooltip("Copy worktree path"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.copy_current_worktree(cx);
                            }))
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_faint)
                                    .child(if worktree_copied {
                                        "Worktree path copied"
                                    } else {
                                        "Current worktree"
                                    }),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(branch),
                            ),
                    )
                    .when(
                        changes_summary.is_some_and(|summary| summary.truncated),
                        |el| {
                            el.child(
                                div()
                                    .rounded_full()
                                    .bg(theme.warning.opacity(0.12))
                                    .px(px(7.0))
                                    .py(px(3.0))
                                    .text_size(px(9.5))
                                    .text_color(theme.warning)
                                    .child("Partial"),
                            )
                        },
                    ),
            )
            .child(popover::menu_separator())
            .child(changes_row)
            .child(popover::menu_separator())
            .child(account_row)
            .child(popover::menu_separator())
            .child(goals_section)
            .into_any_element()
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme_owned = Theme::of(cx).clone();
        let theme = &theme_owned;
        let theme_bg = theme.bg;
        let (border, text, faint) = (theme.border, theme.text, theme.text_faint);

        // Settings route: just the section outlet — the section label lives in
        // the unified window titlebar now (render_title_bar).
        if let Route::Settings(section) = self.route {
            let outlet = self.settings_outlet(section, cx);
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }

        let _ = (text, border);
        let has_selection = self.state.read(cx).selected_chat.is_some();
        let has_spaces = !self.state.read(cx).spaces.is_empty();
        let space_name: SharedString = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.display_name().to_string())
            .unwrap_or_default()
            .into();

        // Content outlet: selected chat → transcript; nothing selected → the
        // "Send a message to start" canvas with a watermark; no spaces at all
        // → the onboarding card. The composer sits below the first two
        // (new-chat mode mints the chat id on first send).
        let outlet: AnyElement = if has_selection {
            self.transcript.clone().into_any_element()
        } else if !has_spaces {
            // Onboarding (first boot / after the destructive wipe): no folders
            // to work in yet — one clear affordance.
            let _ = faint;
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "no-spaces-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            icon(icons::CREW_MARK)
                                .size(px(48.0))
                                .text_color(theme.text.opacity(0.09)),
                        )
                        .child(
                            div()
                                .mt(px(24.0))
                                .text_size(px(16.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from("Add a space to get started")),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .text_size(px(13.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(
                                    "A space is a folder on one of your devices.",
                                )),
                        )
                        .child(
                            popover::btn_primary(&theme_owned, "Add a space")
                                .id("onboarding-add-space")
                                .mt(px(20.0))
                                .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx))),
                        ),
                ))
                .into_any_element()
        } else {
            // New-chat canvas: the dim Crew mark watermark over the centered
            // helper line, naming the space the session will start in.
            let helper: SharedString = if space_name.is_empty() {
                "Send a message to start a new session.".into()
            } else {
                format!("Send a message to start a session in {space_name}.").into()
            };
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "new-chat-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            icon(icons::CREW_MARK)
                                .size(px(48.0))
                                .text_color(theme.text.opacity(0.09)),
                        )
                        .child(
                            div()
                                .mt(px(24.0))
                                .text_size(px(14.0))
                                .text_color(theme.text_muted.opacity(0.6))
                                .child(helper),
                        ),
                ))
                .into_any_element()
        };

        let status = self.render_status_strip(cx);
        // File dropzone over the ENTIRE conversation column (transcript +
        // composer, not just the pill): dragging OS files anywhere across the
        // chat area shows the "Drop files to attach" veil; a drop stages the
        // files in the composer. `has_active_drag` gates the veil so a drag
        // that left the window (FileDrop Exited) can't strand it.
        let file_drag_active = self.file_drag_active && cx.has_active_drag();
        let conversation = div()
            .id("chat-dropzone")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .on_drag_move::<gpui::ExternalPaths>(cx.listener(
                |this, e: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                    let inside = e.bounds.contains(&e.event.position);
                    if this.file_drag_active != inside {
                        this.file_drag_active = inside;
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                this.file_drag_active = false;
                let paths = paths.paths().to_vec();
                this.composer
                    .update(cx, |composer, cx| composer.add_paths(paths, cx));
                cx.notify();
            }))
            .child(
                // The conversation fades out at its bottom edge instead of
                // hard-cutting against the composer — a gradient overlay from
                // transparent into the panel background.
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(outlet)
                    .child(
                        div()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right(px(10.0))
                            .h(px(40.0))
                            .bg(gpui::linear_gradient(
                                0.0,
                                gpui::linear_color_stop(theme_bg, 0.0),
                                gpui::linear_color_stop(theme_bg.opacity(0.0), 1.0),
                            )),
                    )
                    .children(self.render_jump_to_bottom(cx))
                    .children(self.render_remote_presence(cx)),
            )
            // Reserved status strip (h-6) — the WorkingIndicator lives here so
            // the composer below never shifts. Both live INSIDE the
            // conversation region, ABOVE the terminal dock (comet __root.tsx:
            // the terminal panel sits below the whole conversation column).
            .child(status)
            .when(should_show_composer(has_spaces, has_selection), |el| {
                el.child(self.composer.clone())
            })
            .child(self.render_terminal_container(cx))
            .when(file_drag_active, |el| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(theme.scrim().opacity(0.4 / 0.6))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child("Drop files to attach"),
                )
            });
        let workspace_status = has_selection.then(|| self.render_workspace_status(cx));
        div()
            .id("chat-layout")
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_row()
            .overflow_hidden()
            .child(conversation)
            .when_some(workspace_status, |el, status| {
                el.child(
                    div()
                        .id("workspace-status-rail")
                        .w(px(WORKSPACE_STATUS_RAIL_WIDTH))
                        .h_full()
                        .flex_none()
                        .pt(px(10.0))
                        .pr(px(12.0))
                        .pl(px(8.0))
                        .child(status),
                )
            })
            .into_any_element()
    }

    /// The "↓ Scroll to bottom" pill (round-9 §3): a LABELED rounded-full
    /// chip — down-arrow glyph + 13px label on a near-opaque raised surface
    /// with a hairline — horizontally centered over the transcript column and
    /// floating a small gap above the composer. It hangs 14px below the
    /// conversation region (through the reserved h-6 status strip, whose
    /// content is left-aligned) so its bottom edge sits ~10px above the pill.
    /// Shown past the transcript's 320px threshold; 180ms fade + 2px rise in.
    fn render_jump_to_bottom(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.transcript.read(cx).jump_button_shown() {
            return None;
        }
        let theme = Theme::of(cx);
        Some(
            div()
                .absolute()
                .bottom(px(-14.0))
                .left_0()
                .right(px(10.0))
                .flex()
                .justify_center()
                .child(motion::dialog_in(
                    "jump-to-bottom",
                    div()
                        .id("jump-to-bottom-btn")
                        .h(px(30.0))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border)
                        .shadow_md()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .pl(px(11.0))
                        .pr(px(13.0))
                        .cursor_pointer()
                        // Hover must BRIGHTEN the opaque pill, never replace it
                        // with a translucent wash (a 10%-alpha bg here made the
                        // pill go see-through on hover — user-reported), and it
                        // fades over the CSS transition-colors 150ms, not snaps.
                        .bg(motion::hover_blend(
                            "jump-pill",
                            theme.surface_raised,
                            theme.surface_raised_hover,
                        ))
                        .on_hover(motion::hover_listener("jump-pill"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.transcript
                                .update(cx, |transcript, cx| transcript.jump_to_bottom(cx));
                        }))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("↓")),
                        )
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(theme.text)
                                .child(SharedString::from("Scroll to bottom")),
                        ),
                ))
                .into_any_element(),
        )
    }

    /// Terminal panel dock at the main-column bottom: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    fn render_terminal_container(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.terminal_target(cx);
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            return gpui::Empty.into_any_element();
        }
        // Defensive: an open flag needs its entity (and set_open) even if
        // toggle_terminal never created one.
        if self.terminal_open(cx) && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        let border = Theme::of(cx).border;
        let handle_hover = Theme::of(cx).border_strong;
        let height = self.settings.terminal_height;

        let handle = div()
            .id("terminal-resize")
            .h(px(5.0))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .hover(move |s| s.bg(handle_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.terminal_drag_anchor =
                        Some((f32::from(event.position.y), this.settings.terminal_height));
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes).
        let inner = div()
            .h(px(height))
            .w_full()
            .flex()
            .flex_col()
            .child(handle)
            .child(div().flex_1().min_h_0().child(panel));

        div()
            .w_full()
            .flex_none()
            .overflow_hidden()
            .border_t_1()
            .border_color(border)
            .h(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// Working indicator strip: animated activity text + elapsed time,
    /// staleness-gated via [`Indicator`]; falls back to a sending bridge and
    /// then the engine mode line.
    fn render_status_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let state = self.state.read(cx);

        // Aligned with the composer column: centered, same max width, small
        // inner gutter (comet's `mx-auto h-6 max-w-3xl px-2`).
        let strip = div()
            .h(px(Theme::STATUS_STRIP_HEIGHT))
            .flex_none()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG + 8.0))
            .text_size(px(11.0));

        let Some(chat_id) = state.selected_chat.clone() else {
            return strip.into_any_element();
        };
        if state.scaffold_chat_starting(&chat_id) {
            let delta = motion::pulse_delta(&COMET_PULSE, cx.entity_id(), cx);
            return strip
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .opacity(0.6 + 0.35 * delta)
                        .child(SharedString::from("Starting Scaffold sandbox…")),
                )
                .into_any_element();
        }
        let local_indicator = state.indicator_for(&chat_id, now);
        let indicator = if local_indicator == Indicator::None {
            state.selected_agent_indicator(now)
        } else {
            local_indicator
        };
        let elapsed_secs = state
            .session_for(&chat_id)
            .and_then(|s| s.started_at)
            .map(|t| now.signed_duration_since(t).num_seconds())
            .or_else(|| {
                state
                    .selected_agent_session()
                    .filter(|session| session.chat_id == chat_id)
                    .map(|session| {
                        now.timestamp_millis()
                            .saturating_sub(session.created_at)
                            .max(0)
                            / 1_000
                    })
            })
            .unwrap_or(0);
        let sending = self.composer.read(cx).is_sending();

        match indicator {
            Indicator::Working => {
                let word =
                    transcript::flavour_word(transcript::flavour_seed(&chat_id), elapsed_secs);
                let delta = motion::pulse_delta(&COMET_PULSE, cx.entity_id(), cx);
                strip
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .opacity(0.6 + 0.35 * delta)
                            .child(SharedString::from(format!("{word}…"))),
                    )
                    .child(
                        div()
                            .text_color(theme.text_faint)
                            .child(SharedString::from(transcript::format_elapsed(elapsed_secs))),
                    )
                    .into_any_element()
            }
            // No label: the QuestionPanel right below IS the awaiting-input
            // surface — a strip caption above it was redundant (user request).
            Indicator::AwaitingInput => strip.into_any_element(),
            Indicator::Errored => strip
                .text_color(theme.danger)
                .child(SharedString::from("Run failed"))
                .into_any_element(),
            Indicator::None if sending => {
                let delta = motion::pulse_delta(&COMET_PULSE, cx.entity_id(), cx);
                strip
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .opacity(0.6 + 0.35 * delta)
                            .child(SharedString::from("Sending…")),
                    )
                    .into_any_element()
            }
            Indicator::None => strip.into_any_element(),
        }
    }

    /// Right "Changes" pane — hidden by default, drag-resizable; content is the
    /// lazy [`Changes`] diff viewer (created on first open).
    fn render_right_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;
        let content: AnyElement = if self.right_pane_open(cx) {
            let changes = self.changes_pane(cx);
            // Idempotent — also covers a persisted-open pane on boot.
            changes.update(cx, |changes, cx| changes.ensure_watch(cx));
            changes.into_any_element()
        } else {
            gpui::Empty.into_any_element()
        };
        // Its OWN inset card (user request): the conversation card's right
        // gutter is the gap; padding (not margins) keeps the tweened width
        // container clean, and the resize grabber floats over the gap.
        let handle = self
            .resize_handle(
                "right-pane-resize",
                || RightPaneResize,
                |shell, _| shell.settings.right_pane_width = RIGHT_PANE_DEFAULT,
                cx,
            )
            .absolute()
            .top_0()
            .bottom_0()
            // INSIDE the width-clipped container (a negative inset was
            // clipped into unreachability — user-reported dead resize),
            // overlapping the card's left border.
            .left(px(0.0));
        let card = div()
            .size_full()
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(bg)
            .overflow_hidden()
            .child(content);
        let target = self.right_target(cx);
        self.pane_container(
            self.right_tween,
            target,
            // Mirrors the conversation card's box exactly: flush under the
            // titlebar (no top pad), 8px bottom/right gutters — the
            // conversation card's own right margin is the 8px gap between the
            // two insets (user-reported height/gap mismatch).
            div()
                .h_full()
                .relative()
                .pb(px(8.0))
                .pr(px(8.0))
                .child(card)
                .child(handle)
                .into_any_element(),
        )
    }

    fn render_gate_card(&mut self, phase: &GatePhase, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let content: AnyElement = match phase {
            // Backend unreachable: quiet centered copy (comet Gate `Failed`),
            // plus a Retry affordance (the native engine doesn't self-redial).
            GatePhase::Failed(error) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(Theme::SPACE_MD))
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(error.clone())),
                )
                .child(
                    div()
                        .id("retry-engine")
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.border)
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.glass_hover()))
                        .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            // Login card: centered on the grid with the Crew mark and
            // one full-width sign-in action.
            _ => div()
                .w(px(360.0))
                .px(px(32.0))
                .py(px(40.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface_card)
                .shadow_lg()
                .flex()
                .flex_col()
                .items_center()
                .text_center()
                .child(
                    icon(icons::CREW_MARK)
                        .size(px(36.0))
                        .text_color(theme.text),
                )
                .child(
                    div()
                        .mt(px(24.0))
                        .text_size(px(18.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from("Log in to Crew")),
                )
                .child(
                    div()
                        .mt(px(6.0))
                        .mb(px(24.0))
                        .text_size(px(13.0))
                        .line_height(px(19.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            "This opens your browser to finish logging in — you'll come right back.",
                        )),
                )
                .child(
                    div()
                        .id("sign-in")
                        .w_full()
                        .h(px(36.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .bg(theme.text)
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.on_solid)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.9))
                        .on_click(cx.listener(|this, _, _, cx| this.start_sign_in(cx)))
                        .child(SharedString::from("Log in")),
                )
                .into_any_element(),
        };
        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Keyed per phase (comet App.tsx `<div key={phase}
                    // className="animate-in">`): every gate swap replays the
                    // 0.5s entrance instead of mutating one animated element.
                    .child(motion::fade_in(
                        match phase {
                            GatePhase::SignIn => "gate-card-signin",
                            _ => "gate-card-failed",
                        },
                        div().child(content),
                    )),
            )
            .into_any_element()
    }
}

/// The sign-in gate's faint grid backdrop (comet styles.css `.bg-grid`):
/// 44px hairlines at white 3.5%, with the radial mask approximated by edge
/// gradients back into the page background (gpui has no mask-image).
fn grid_backdrop(theme: &Theme) -> AnyElement {
    let line = crate::theme::hairline(0.035);
    let bg = theme.bg;
    const STEP: f32 = 44.0;
    const SPAN: f32 = 2640.0;
    let verticals = (1..(SPAN / STEP) as usize).map(|i| {
        div()
            .absolute()
            .left(px(i as f32 * STEP))
            .top_0()
            .bottom_0()
            .w(px(1.0))
            .bg(line)
    });
    let horizontals = (1..((SPAN * 0.75) / STEP) as usize).map(|i| {
        div()
            .absolute()
            .top(px(i as f32 * STEP))
            .left_0()
            .right_0()
            .h(px(1.0))
            .bg(line)
    });
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .children(verticals)
        .children(horizontals)
        // Mask approximation: fade the grid back into the background toward
        // the window edges (the original masks to an ellipse at 50% / 40%).
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(120.0))
                .bg(gpui::linear_gradient(
                    180.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(px(260.0))
                .bg(gpui::linear_gradient(
                    0.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    90.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    270.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .into_any_element()
}

/// A size-6 icon button for the titlebar strip (comet window-controls.tsx:
/// `grid size-6 place-items-center rounded-md text-muted-foreground`).
fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("window-control-{id}");
    div()
        .id(id)
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // comet window-controls.tsx: `transition-colors` — the wash fades.
        .bg(motion::hover_blend(
            &fade_key,
            theme.glass_hover().opacity(0.0),
            theme.glass_hover(),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Buttons in/over a titlebar drag strip must be EXCLUDED from the
        // strip's event surface entirely. `.occlude()` (gpui
        // `HitboxBehavior::BlockMouse`) makes the window hit-test STOP at the
        // button, so every `is_hovered`-guarded strip listener — the
        // mouse-down that arms the drag, the mouse-move that hands AppKit a
        // native drag session (`performWindowDragWithEvent:`, whose second
        // quick click zooms NATIVELY on macOS), and the `click_count == 2`
        // zoom handler — never fires with the pointer over a button. It also
        // removes the button's rect from the native Drag control-area
        // hit-test on Windows/Linux. Click-level propagation is also stopped.
        // Double-click on empty strip space still zooms.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

const WINDOWS_CAPTION_BUTTON_WIDTH: f32 = 36.0;
const WINDOWS_CAPTION_WIDTH: f32 = WINDOWS_CAPTION_BUTTON_WIDTH * 3.0;

fn titlebar_right_padding(is_windows: bool, base: f32) -> f32 {
    base + if is_windows {
        WINDOWS_CAPTION_WIDTH
    } else {
        0.0
    }
}

/// A Windows-owned caption target using GPUI's native non-client hit-test
/// areas and the platform's system glyphs.
fn windows_caption_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    theme: &Theme,
    close: bool,
) -> impl IntoElement {
    let (hover_bg, hover_fg, active_bg, active_fg) = if close {
        let red: gpui::Hsla = gpui::rgb(0xe81123).into();
        (
            red,
            gpui::white(),
            red.opacity(0.8),
            gpui::white().opacity(0.8),
        )
    } else {
        (
            theme.glass_hover(),
            theme.text,
            theme.glass_hover().opacity(0.7),
            theme.text,
        )
    };
    div()
        .id(id)
        .w(px(WINDOWS_CAPTION_BUTTON_WIDTH))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.0))
        .text_color(theme.text)
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .active(move |style| style.bg(active_bg).text_color(active_fg))
        .occlude()
        .window_control_area(area)
        .child(glyph)
}

/// A titlebar history button (comet window-controls.tsx): enabled it is a
/// normal window-control button; disabled it dims to 35% opacity and ignores
/// the pointer (`disabled:pointer-events-none disabled:opacity-35`).
fn nav_history_button(
    id: &'static str,
    icon_path: &'static str,
    enabled: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    if !enabled {
        return div()
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            // Even disabled it reads as a control — occlude so double-clicks
            // on it don't fall through to the titlebar strip's zoom handler.
            .occlude()
            .child(
                icon(icon_path)
                    .size(px(16.0))
                    .text_color(theme.text_muted.opacity(0.35)),
            )
            .into_any_element();
    }
    window_control_button(id, icon_path, theme, on_click).into_any_element()
}

/// A size-7 icon button for the main-panel header (comet __root.tsx:
/// `grid size-7 place-items-center rounded-md text-muted-foreground`).
fn header_icon_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("header-icon-{id}");
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // comet __root.tsx header buttons: `transition-colors`.
        .bg(motion::hover_blend(
            &fade_key,
            crate::theme::wash(0.0),
            crate::theme::wash(0.11),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Same occlusion + click-swallowing as [`window_control_button`]: this
        // button sits inside the chat header's titlebar drag region, so its
        // rect must be carved out of the strip's drag/double-click surface.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let close_completed_session_import = {
            let target_chat = self.session_import_target_chat.as_deref();
            if !self.session_import_open {
                false
            } else {
                let state = self.state.read(cx);
                let target_still_available = target_chat.is_some_and(|target| {
                    state
                        .local_session_candidates
                        .iter()
                        .any(|candidate| candidate.chat_id == target)
                });
                local_session_import_completed(
                    target_chat,
                    state.selected_chat.as_deref(),
                    target_still_available,
                )
            }
        };
        if close_completed_session_import {
            self.session_import_open = false;
            self.session_import_target_chat = None;
        }
        let theme = Theme::of(cx);
        // The shell tone (comet `.frost`): the surface the sidebar sits on and
        // the main panel floats over as an inset rounded card. On macOS the
        // window background is the blurred desktop (lib.rs `Blurred`), so the
        // frost paints translucent — the sidebar and card margins read as
        // glass while the opaque card keeps text off it.
        let (frost, text, font) = (theme.glass(), theme.text, theme.font_sans.clone());
        let gate = self
            .debug_gate
            .clone()
            .unwrap_or_else(|| self.state.read(cx).gate());

        // Fullscreen hides the macOS traffic lights — reflow the control
        // cluster with a 200ms ease-out tween (§1.1). A fullscreen transition
        // resizes the window, which re-renders us, so polling here is exact.
        let fullscreen = window.is_fullscreen();
        if self.fullscreen != Some(fullscreen) {
            if self.fullscreen.is_some() && cfg!(target_os = "macos") {
                self.titlebar_tween = Some(WidthTween::new(
                    titlebar_cluster_start(!fullscreen),
                    titlebar_cluster_start(fullscreen),
                ));
            }
            self.fullscreen = Some(fullscreen);
        }
        // Manual tween drive bookkeeping for this pass (see [`WidthTween`]).
        self.reduced_motion = motion::reduced_motion(cx);
        self.motion_active.set(false);

        // Keyboard shortcuts (mod-s/b/j) dispatch through the window focus
        // chain — with nothing focused they go dead. Land initial focus on the
        // composer, and whenever focus is lost with no successor (e.g. the
        // focused element unmounted), route it back there.
        if self.focus_sub.is_none() {
            self.focus_sub = Some(cx.on_focus_lost(window, |this: &mut Shell, window, cx| {
                match this.route {
                    Route::Chat => window.focus(&this.composer.focus_handle(cx), cx),
                    // No composer here — clear the stale handle so `focused()`
                    // reads None (the render hook below re-lands focus when the
                    // route returns to Chat; a lingering unmounted handle would
                    // otherwise dead-end keyboard dispatch for good).
                    Route::Settings(_) => window.blur(),
                }
            }));
        }
        if std::mem::take(&mut self.annotation_focus_pending)
            && let Some(inspector) = &self.annotation_inspector
        {
            window.focus(&inspector.input.focus_handle(cx), cx);
        }
        if std::mem::take(&mut self.annotation_submit_pending)
            && self.annotation_inspector.is_some()
        {
            // Save mutates the composer and moves focus — deferred out of the
            // draw rather than run mid-render.
            let shell = cx.entity();
            window.defer(cx, move |window, cx| {
                shell.update(cx, |shell, cx| shell.save_annotation(window, cx));
            });
        }
        if matches!(gate, GatePhase::Ready)
            && matches!(self.route, Route::Chat)
            && window.focused(cx).is_none()
        {
            window.focus(&self.composer.focus_handle(cx), cx);
        }

        let root = div()
            .id("shell-root")
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(frost)
            .text_color(text)
            .font_family(font)
            .text_size(px(14.0))
            .on_drag_move(cx.listener(Self::on_sidebar_drag))
            .on_drag_move(cx.listener(Self::on_right_pane_drag))
            .on_drag_move(cx.listener(Self::on_terminal_drag))
            // The panel shortcuts are chat-scoped chrome: in Settings they are
            // no-ops (comet __root.tsx gates the hotkey on `!isSettings`, and
            // the terminal panel is only mounted on session routes). The
            // sidebar toggle stays live everywhere, as in the original.
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_terminal(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_right_pane(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &AddSpacePalette, _, cx| {
                if this.add_space.is_some() {
                    this.add_space = None;
                    cx.notify();
                } else {
                    this.open_add_space(cx);
                }
            }))
            .on_action(
                cx.listener(|this, _: &ToggleCommandPalette, _, cx| {
                    this.toggle_command_palette(cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ToggleActivity, _, cx| this.toggle_activity(cx)))
            .on_action(cx.listener(|this, _: &ToggleFocusMode, _, cx| this.toggle_focus_mode(cx)))
            .on_action(cx.listener(|this, _: &OpenInvite, _, cx| this.open_invite(cx)))
            .on_action(cx.listener(|this, _: &NewSession, _, cx| {
                this.open_new_session(cx);
            }))
            .on_action(cx.listener(|this, _: &CloseSession, window, cx| {
                this.close_active_session(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &crate::composer::MentionEscape, _, cx| {
                    this.handle_escape(cx);
                }),
            );

        let root = match &gate {
            GatePhase::Ready => {
                // Focus is a sync signal: on the rising edge of window
                // activation, nudge every open room to verify liveness — a
                // broadcast-deaf socket (accepted writes, runtime pongs,
                // nothing delivered; 2026-08-04 incident) then heals within
                // seconds of the user looking at the app rather than waiting
                // out the background probe cadence.
                let window_active = window.is_window_active();
                if window_active && !self.was_window_active {
                    self.state.update(cx, |s, cx| s.probe_sync(cx));
                }
                self.was_window_active = window_active;
                // A run finishing while you're LOOKING at the session must not
                // badge "completed" until you leave and return — mark it seen
                // live while the window is active (idempotent guard inside;
                // one extra frame settles it).
                if window_active {
                    let unseen_selected = {
                        let s = self.state.read(cx);
                        s.selected_chat_row()
                            // An explicit "mark as unread" pin outranks the
                            // looking-at-it stamp until the user re-selects.
                            .filter(|c| c.unseen() && !s.chat_marked_unread(&c.id))
                            .map(|c| c.id.clone())
                    };
                    if let Some(chat_id) = unseen_selected {
                        self.state
                            .update(cx, |s, cx| s.mark_chat_seen(&chat_id, cx));
                    }
                }
                // Capture knob: `COMET_OPEN_DIALOG=model` pops the combined
                // harness/model menu (needs `window`, so it fires here rather
                // than in `on_state_changed`).
                if self.debug_dialog.as_deref() == Some("model") {
                    self.debug_dialog = None;
                    self.composer
                        .update(cx, |c, cx| c.debug_open_model_menu(window, cx));
                }
                // MessageRail width gate: hide below 48rem of main-panel width.
                let viewport = f32::from(window.viewport_size().width);
                let main_width = viewport - self.sidebar_target() - self.right_target(cx) - 10.0;
                self.transcript.update(cx, |t, cx| {
                    t.set_rail_enabled(rail::rail_visible(main_width), cx)
                });

                let sidebar = self.render_sidebar(cx);
                let sidebar_handle = self.resize_handle(
                    "sidebar-resize",
                    || SidebarResize,
                    |shell, _| shell.settings.sidebar_width = SIDEBAR_DEFAULT,
                    cx,
                );
                let main = self.render_main(cx);
                // The Changes pane is chat-scoped chrome: the Settings route
                // never renders it (comet __root.tsx `!isSettings && activeChat`
                // around the diff column) — the per-session open flags stay
                // intact for the return trip.
                let on_chat = matches!(self.route, Route::Chat);
                let right: AnyElement = if on_chat {
                    self.render_right_pane(cx)
                } else {
                    Empty.into_any_element()
                };
                let overlays = self.render_overlays(window.viewport_size(), window, cx);
                // The signature frame: the conversation card and — when the
                // changes pane is open — a SECOND inset card beside it, both
                // rounded hairline-bordered floats on the frost shell (the
                // changes card is built inside `render_right_pane`).
                let theme = Theme::of(cx);
                // Margins, radius, and border-color MELT over the same 200ms
                // ease-out as the sidebar width (comet __root.tsx `<main>`
                // `transition-[margin,border-radius,border-color]`; collapsed
                // is `m-0 rounded-none border-transparent` — the border WIDTH
                // stays, only its color fades, so layout never jumps by the
                // hairline).
                let border_color = theme.border;
                let card = div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .bg(theme.bg)
                    .border_1()
                    .child(main);
                // Manual drive on the SAME clock as the sidebar width tween.
                // Crucially there is no `with_animation` wrapper here: the
                // wrapper's epoch-keyed id used to change every card
                // descendant's global element-id path on each toggle, which
                // reset gpui's per-element animation state and REPLAYED any
                // stale pane/terminal tween from t=0 (the changes pane slid
                // ~100px under the clip mid-toggle — round-6 §2/§3).
                //
                // The inset card persists in EVERY state (user request): top
                // gutter under the unified titlebar, constant left/right/
                // bottom gutters, constant radius + hairline — the 8px left
                // gap holds whether it borders the sidebar or the window edge.
                // No top margin: the titlebar's own internal air (44px bar,
                // 28px tabs) is the gap — an extra gutter read as a hole
                // between the header and the app (user report).
                // The right margin is the window gutter when the changes
                // pane is closed, but the SEAM between the two inset cards
                // when it's open — a full gutter there read double-wide next
                // to the two borders it separates (user report).
                let right_gap = if on_chat && self.right_pane_open(cx) {
                    4.0
                } else {
                    8.0
                };
                let card: AnyElement = card
                    .mb(px(8.0))
                    .mr(px(right_gap))
                    .ml(px(8.0))
                    .rounded(px(12.0))
                    .border_color(border_color)
                    .into_any_element();
                // The whole app page is one keyed `animate-in` entrance (comet
                // App.tsx `<div key={phase} className="animate-in h-full">`):
                // arriving from the splash or any gate fades the page in; the
                // splash-out crossfades over it on boot.
                // The sidebar resize handle FLOATS over the sidebar/card seam
                // (zero layout width, same idiom as the changes-pane grabber)
                // so the sidebar's right gutter stays exactly as wide as its
                // left one — a 5px flex child here read as lopsided spacing.
                let sidebar_seam = div()
                    .w(px(0.0))
                    .h_full()
                    .flex_none()
                    .relative()
                    .child(sidebar_handle.absolute().top_0().bottom_0().left(px(-2.0)));
                let title_bar = self.render_title_bar(cx);
                // Sidebar tone: a slightly lighter column behind the sidebar,
                // spanning the FULL window height (under the traffic lights,
                // through the titlebar, down to the bottom edge). Its width
                // rides the same tween as the sidebar, so the tone melts away
                // with the collapse instead of vanishing in a frame.
                let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
                // Hairline on its right edge — full height like the tone,
                // so the sidebar column reads as its own surface.
                let sidebar_tone = div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(sidebar_now))
                    .bg(crate::theme::wash(0.05))
                    .border_r_1()
                    .border_color(border_color);
                let page = div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(title_bar)
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_row()
                            .child(sidebar)
                            .child(sidebar_seam)
                            .child(card)
                            .child(right),
                    )
                    .child(self.render_titlebar_cluster(cx))
                    .children(overlays);
                root.child(sidebar_tone)
                    .child(motion::fade_in("phase-app", page))
            }
            GatePhase::Loading => root, // splash overlay covers boot
            phase @ (GatePhase::Failed(_) | GatePhase::SignIn) => {
                let card = self.render_gate_card(phase, cx);
                root.child(card)
            }
        };

        // A manually-driven tween is mid-flight: keep frames coming (the same
        // scheduling `with_animation` would have requested). Hover color fades
        // ride the same clock; their once-per-frame tick lives here (this is
        // the window's root render — it runs exactly once per frame).
        if self.motion_active.get() | motion::hover_fades_active() {
            window.request_animation_frame();
        }

        // Boot splash overlay: visible → crossfades out on Ready → removed.
        let root = match self.splash {
            SplashPhase::Visible => {
                let theme = Theme::of(cx).clone();
                let view = cx.entity_id();
                root.child(loaders::splash_overlay(&theme, false, view, cx))
            }
            SplashPhase::FadingOut => {
                let theme = Theme::of(cx).clone();
                let view = cx.entity_id();
                root.child(loaders::splash_overlay(&theme, true, view, cx))
            }
            SplashPhase::Gone => root,
        };

        // Caption controls are shell-level chrome, above the splash and auth
        // or connection-error gates as well as the full application.
        let root = if matches!(gate, GatePhase::Ready) || !cfg!(target_os = "windows") {
            root
        } else {
            root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(Theme::TITLEBAR_HEIGHT))
                    .window_control_area(WindowControlArea::Drag),
            )
        };
        root.children(self.render_windows_caption_controls(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn ready_before_shell_observation_cannot_leave_boot_splash_visible() {
        assert_eq!(
            next_splash_phase(SplashPhase::Visible, &ConnectionStatus::Ready),
            SplashPhase::FadingOut
        );
    }

    #[test]
    fn selected_existing_chat_does_not_close_import_before_attach_finishes() {
        assert!(!local_session_import_completed(
            Some("chat-a"),
            Some("chat-a"),
            true,
        ));
        assert!(local_session_import_completed(
            Some("chat-a"),
            Some("chat-a"),
            false,
        ));
        assert!(!local_session_import_completed(
            Some("chat-a"),
            Some("chat-b"),
            false,
        ));
    }

    /// A press on the settle button hands its click to the button; the row
    /// underneath must not also select the session.
    #[test]
    fn settle_button_press_takes_the_click_from_row_selection() {
        let mut press = SettlePress::default();
        // One press, dispatched descendant-first: button, then row.
        press.press_button();
        press.press_row();
        assert!(!press.row_click_selects());
    }

    /// The suppression covers exactly the click it belongs to: a settle press
    /// must never swallow a later click on the row body — which is reachable
    /// right after settling, since the row moves to the Settled list.
    #[test]
    fn settle_press_never_suppresses_a_later_row_click() {
        let mut press = SettlePress::default();
        press.press_button();
        press.press_row();
        // Next press lands on the row body — no button press precedes it.
        press.press_row();
        assert!(press.row_click_selects());
    }

    /// Settling twice in a row (toggling the same button) suppresses each of
    /// those clicks, not just the first.
    #[test]
    fn repeated_settle_presses_each_take_their_own_click() {
        let mut press = SettlePress::default();
        for _ in 0..2 {
            press.press_button();
            press.press_row();
            assert!(!press.row_click_selects());
        }
    }

    struct RightDrawerProbe;

    impl Render for RightDrawerProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let viewport = gpui::size(px(800.0), px(600.0));
            div().size_full().child(right_drawer_overlay(
                viewport,
                div()
                    .debug_selector(|| "RIGHT_DRAWER".into())
                    .w(px(360.0))
                    .h(viewport.height),
            ))
        }
    }

    struct SelectionCommentActionProbe {
        underlying_mouse_down: Rc<Cell<bool>>,
        popover_open: Rc<Cell<bool>>,
    }

    impl Render for SelectionCommentActionProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let underlying_mouse_down = self.underlying_mouse_down.clone();
            let popover_open = self.popover_open.clone();
            let is_open = self.popover_open.get();
            div()
                .size_full()
                .child(
                    div()
                        .absolute()
                        .left(px(80.0))
                        .top(px(80.0))
                        .size(px(160.0))
                        .on_mouse_down(MouseButton::Left, move |_, _, _| {
                            underlying_mouse_down.set(true);
                        }),
                )
                .child(
                    gpui::deferred(
                        gpui::anchored()
                            .position(gpui::point(px(100.0), px(100.0)))
                            .anchor(gpui::Anchor::TopLeft)
                            .child(
                                div()
                                    .id("selection-comment-action-probe")
                                    .debug_selector(|| "SELECTION_COMMENT_ACTION".into())
                                    .size(px(120.0))
                                    .occlude()
                                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                        cx.stop_propagation();
                                        popover_open.set(true);
                                        window.refresh();
                                    }),
                            ),
                    )
                    .priority(1),
                )
                .when(is_open, |root| {
                    root.child(popover::menu_at(
                        "selection-comment-popover-probe",
                        gpui::point(px(100.0), px(138.0)),
                        div()
                            .debug_selector(|| "SELECTION_COMMENT_POPOVER".into())
                            .size(px(160.0))
                            .into_any_element(),
                    ))
                })
        }
    }

    #[gpui::test]
    fn right_drawer_overlay_anchors_panel_to_viewport_edge(cx: &mut gpui::TestAppContext) {
        let viewport = gpui::size(px(800.0), px(600.0));
        let window = cx.open_window(viewport, |_, _| RightDrawerProbe);
        cx.run_until_parked();

        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        let bounds = cx
            .debug_bounds("RIGHT_DRAWER")
            .expect("right drawer must render in the deferred overlay layer");
        assert_eq!(bounds.origin, gpui::point(px(440.0), px(0.0)));
        assert_eq!(bounds.size, gpui::size(px(360.0), px(600.0)));
    }

    #[gpui::test]
    fn selection_comment_action_opens_popover_without_clearing_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| Theme::install(crate::theme::Appearance::Dark, cx));
        let underlying_mouse_down = Rc::new(Cell::new(false));
        let popover_open = Rc::new(Cell::new(false));
        let window = cx.open_window(gpui::size(px(400.0), px(300.0)), {
            let underlying_mouse_down = underlying_mouse_down.clone();
            let popover_open = popover_open.clone();
            move |_, _| SelectionCommentActionProbe {
                underlying_mouse_down,
                popover_open,
            }
        });
        cx.run_until_parked();

        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        let bounds = cx
            .debug_bounds("SELECTION_COMMENT_ACTION")
            .expect("selection comment action must render");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(popover_open.get(), "comment action should open its popover");
        assert!(
            cx.debug_bounds("SELECTION_COMMENT_POPOVER").is_some(),
            "comment popover should render after the action is clicked"
        );
        assert!(
            !underlying_mouse_down.get(),
            "comment action must block the transcript hitbox beneath it"
        );
    }

    #[test]
    fn annotation_prompt_context_keeps_selection_and_comment_distinct() {
        let annotation = comet_proto::SemanticAnnotation {
            id: "note-1".into(),
            author_subject: "user-1".into(),
            body: "Use the typed helper here.".into(),
            anchor: comet_proto::SemanticAnchor {
                target_kind: comet_proto::AnchorTargetKind::Message,
                target_id: "message-1".into(),
                file: None,
                byte_range: None,
                exact: Some("unsafe fallback".into()),
                prefix_hash: None,
                suffix_hash: None,
                unknown: Default::default(),
            },
            state: comet_proto::AnnotationState::Anchored,
            created_at: 0,
            resolved_at: None,
            unknown: Default::default(),
        };

        assert_eq!(
            annotation_prompt_context(&annotation),
            "Selected text:\nunsafe fallback\n\nComment:\nUse the typed helper here."
        );
    }

    fn transcript_entry(
        id: &str,
        parts: Vec<comet_doc::MessagePart>,
    ) -> comet_doc::SessionMessageEntry {
        comet_doc::SessionMessageEntry {
            id: id.into(),
            role: comet_doc::MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "device".into(),
            status: Some(comet_doc::MessageStatus::Streaming),
            continuation_of: None,
        }
    }

    #[test]
    fn shell_invalidation_ignores_plain_transcript_text_but_not_goal_projection() {
        let now = Utc::now();
        let mut state = AppState::default();
        let state_key = ShellStateProjection::capture(&state, now);
        let transcript_key = TranscriptChromeProjection::default();

        state.transcript.push(transcript_entry(
            "assistant",
            vec![comet_doc::MessagePart::Text {
                id: "text".into(),
                text: "streaming".into(),
            }],
        ));
        let text_state_key = ShellStateProjection::capture(&state, now);
        assert!(!shell_invalidation_changed(
            &state_key,
            &text_state_key,
            &transcript_key,
            &transcript_key,
        ));

        let mut goal_key = transcript_key.clone();
        goal_key.goal_groups = vec![GoalGroupRows {
            label: None,
            rows: vec![GoalRowData {
                text: "Ship it".into(),
                done: false,
                depth: 0,
            }],
        }];
        assert!(shell_invalidation_changed(
            &state_key,
            &text_state_key,
            &transcript_key,
            &goal_key,
        ));
    }

    #[test]
    fn shell_state_projection_invalidates_navigation_changes() {
        let now = Utc::now();
        let mut state = AppState::default();
        let before = ShellStateProjection::capture(&state, now);
        state.selected_chat = Some("chat-b".into());
        let after = ShellStateProjection::capture(&state, now);
        assert!(shell_invalidation_changed(
            &before,
            &after,
            &TranscriptChromeProjection::default(),
            &TranscriptChromeProjection::default(),
        ));
    }

    #[test]
    fn structured_goal_projection_ignores_text_deltas_and_detects_todo_updates() {
        let mut entry = transcript_entry(
            "assistant",
            vec![comet_doc::MessagePart::Text {
                id: "text".into(),
                text: "a".into(),
            }],
        );
        let cached = goal_entry_projection(&entry);
        let comet_doc::MessagePart::Text { text, .. } = &mut entry.parts[0] else {
            unreachable!();
        };
        text.push('b');
        assert!(goal_entry_projection_matches(&cached, &entry));

        let plain_append = transcript_entry(
            "assistant-2",
            vec![comet_doc::MessagePart::Text {
                id: "text-2".into(),
                text: "more".into(),
            }],
        );
        let plain_projection = goal_entry_projection(&plain_append);
        assert!(plain_projection.is_empty());
        assert!(!goal_entry_ranges_differ(&[], &[plain_projection]));

        let todo = transcript_entry(
            "todo",
            vec![comet_doc::MessagePart::Tool {
                id: "todo-call".into(),
                call: comet_proto::ToolCall::Todo {
                    items: vec![comet_proto::TodoItem {
                        text: "Ship it".into(),
                        done: false,
                    }],
                },
                is_error: false,
                resolved: true,
            }],
        );
        let todo_projection = goal_entry_projection(&todo);
        assert!(!todo_projection.is_empty());
        assert!(!goal_entry_projection_matches(&cached, &todo));
        assert!(goal_entry_ranges_differ(&[cached], &[todo_projection]));
    }

    #[test]
    fn latest_active_omp_goal_obeys_the_newest_goal_update() {
        let goal_entry = |id: &str, goal: serde_json::Value| comet_doc::SessionMessageEntry {
            id: id.into(),
            role: comet_doc::MessageRole::Assistant,
            parts: vec![comet_doc::MessagePart::Tool {
                id: comet_proto::OMP_GOAL_STATE_CALL_ID.into(),
                call: comet_proto::ToolCall::Unknown {
                    name: comet_proto::OMP_GOAL_STATE_CALL_NAME.into(),
                    input: Some(serde_json::json!({ "goal": goal })),
                },
                is_error: false,
                resolved: false,
            }],
            created_at: 0,
            device_id: "device".into(),
            status: Some(comet_doc::MessageStatus::Complete),
            continuation_of: None,
        };
        let active = goal_entry(
            "active",
            serde_json::json!({
                "id": "g1",
                "objective": " Ship the release ",
                "status": "active"
            }),
        );
        assert_eq!(
            latest_active_omp_goal(std::slice::from_ref(&active)),
            Some(ActiveHarnessGoal {
                objective: "Ship the release".into(),
                status: "active".into(),
            })
        );

        let dropped = goal_entry("dropped", serde_json::Value::Null);
        assert_eq!(latest_active_omp_goal(&[active, dropped]), None);
    }

    #[test]
    fn latest_goal_items_uses_the_newest_persisted_todo_snapshot() {
        let todo_entry =
            |id: &str, items: Vec<comet_proto::TodoItem>| comet_doc::SessionMessageEntry {
                id: id.into(),
                role: comet_doc::MessageRole::Assistant,
                parts: vec![comet_doc::MessagePart::Tool {
                    id: "omp-plan".into(),
                    call: comet_proto::ToolCall::Todo { items },
                    is_error: false,
                    resolved: true,
                }],
                created_at: 0,
                device_id: "device".into(),
                status: Some(comet_doc::MessageStatus::Complete),
                continuation_of: None,
            };
        let entries = vec![
            todo_entry(
                "old",
                vec![comet_proto::TodoItem {
                    text: "Old goal".into(),
                    done: false,
                }],
            ),
            todo_entry(
                "new",
                vec![
                    comet_proto::TodoItem {
                        text: "First".into(),
                        done: true,
                    },
                    comet_proto::TodoItem {
                        text: "Second".into(),
                        done: false,
                    },
                ],
            ),
        ];

        assert_eq!(
            latest_goal_items(&entries),
            Some(
                [
                    comet_proto::TodoItem {
                        text: "First".into(),
                        done: true,
                    },
                    comet_proto::TodoItem {
                        text: "Second".into(),
                        done: false,
                    },
                ]
                .as_slice()
            )
        );

        let cleared = vec![todo_entry("cleared", Vec::new())];
        assert_eq!(latest_goal_items(&cleared), Some([].as_slice()));
    }

    #[test]
    fn structured_goal_groups_replay_phases_and_updates() {
        let tool_entry = |id: &str, input: serde_json::Value| comet_doc::SessionMessageEntry {
            id: id.into(),
            role: comet_doc::MessageRole::Assistant,
            parts: vec![comet_doc::MessagePart::Tool {
                id: id.into(),
                call: comet_proto::ToolCall::Unknown {
                    name: "todo".into(),
                    input: Some(input),
                },
                is_error: false,
                resolved: true,
            }],
            created_at: 0,
            device_id: "device".into(),
            status: Some(comet_doc::MessageStatus::Complete),
            continuation_of: None,
        };
        let entries = vec![
            tool_entry(
                "init",
                serde_json::json!({
                    "op": "init",
                    "list": [
                        {"phase": "Layout", "items": ["Inspect", "Implement"]},
                        {"phase": "Verification", "items": ["Build"]}
                    ]
                }),
            ),
            tool_entry(
                "done-task",
                serde_json::json!({"op": "done", "task": "Inspect"}),
            ),
            tool_entry(
                "append",
                serde_json::json!({
                    "op": "append",
                    "phase": "Verification",
                    "items": ["Smoke"]
                }),
            ),
            tool_entry(
                "done-phase",
                serde_json::json!({"op": "done", "phase": "Verification"}),
            ),
        ];

        assert_eq!(
            structured_goal_groups(&entries),
            Some(vec![
                GoalGroupData {
                    label: Some("Layout".into()),
                    items: vec![
                        comet_proto::TodoItem {
                            text: "Inspect".into(),
                            done: true,
                        },
                        comet_proto::TodoItem {
                            text: "Implement".into(),
                            done: false,
                        },
                    ],
                },
                GoalGroupData {
                    label: Some("Verification".into()),
                    items: vec![
                        comet_proto::TodoItem {
                            text: "Build".into(),
                            done: true,
                        },
                        comet_proto::TodoItem {
                            text: "Smoke".into(),
                            done: true,
                        },
                    ],
                },
            ])
        );
    }

    #[test]
    fn goal_rows_preserve_nested_lists_and_continuation_text() {
        let items = vec![
            comet_proto::TodoItem {
                text: "Release\n- [x] Build\n  1. [ ] Verify narrow viewport\n- Ship".into(),
                done: false,
            },
            comet_proto::TodoItem {
                text: "Document the status\nwithout splitting prose".into(),
                done: true,
            },
        ];

        assert_eq!(
            goal_rows(&items),
            vec![
                GoalRowData {
                    text: "Release".into(),
                    done: false,
                    depth: 0,
                },
                GoalRowData {
                    text: "Build".into(),
                    done: true,
                    depth: 1,
                },
                GoalRowData {
                    text: "Verify narrow viewport".into(),
                    done: false,
                    depth: 2,
                },
                GoalRowData {
                    text: "Ship".into(),
                    done: false,
                    depth: 1,
                },
                GoalRowData {
                    text: "Document the status without splitting prose".into(),
                    done: true,
                    depth: 0,
                },
            ]
        );
    }

    #[test]
    fn local_session_capability_uses_native_ownership_flags() {
        let resumable = LocalSessionCapability::from_flags(true, false);
        assert_eq!(resumable, LocalSessionCapability::Resume);
        assert_eq!(
            (resumable.source_label(false), resumable.action_label(false)),
            ("Existing", "Open")
        );

        let history = LocalSessionCapability::from_flags(false, true);
        assert_eq!(history, LocalSessionCapability::ImportHistory);
        assert_eq!(
            (history.source_label(false), history.action_label(false)),
            ("History only", "Import")
        );
        assert_eq!(resumable.action_label(true), "Open");
        assert_eq!(
            (
                LocalSessionCapability::Unavailable.source_label(true),
                LocalSessionCapability::Unavailable.action_label(true)
            ),
            ("Running", "In use")
        );

        assert_eq!(
            LocalSessionCapability::from_flags(false, false),
            LocalSessionCapability::Unavailable
        );
        assert_eq!(
            LocalSessionCapability::from_flags(true, true),
            LocalSessionCapability::Unavailable
        );
    }

    fn local_candidate(
        id: &str,
        harness: comet_proto::HarnessId,
        updated_at: i64,
    ) -> comet_proto::LocalSessionCandidate {
        comet_proto::LocalSessionCandidate {
            id: id.into(),
            chat_id: format!("chat-{id}"),
            harness,
            session_id: format!("native-{id}"),
            cwd: "/workspace".into(),
            title: id.into(),
            preview: None,
            model: None,
            reasoning: None,
            created_at: updated_at - 1,
            updated_at,
            live_attachable: false,
            resumable: true,
            history_only: false,
            busy_elsewhere: None,
        }
    }

    #[test]
    fn local_session_import_groups_nonempty_providers_in_display_order() {
        let sections = local_session_provider_sections(&[
            local_candidate("codex-new", comet_proto::HarnessId::Codex, 30),
            local_candidate("omp-old", comet_proto::HarnessId::Omp, 10),
            local_candidate("omp-new", comet_proto::HarnessId::Omp, 40),
            local_candidate("codex-old", comet_proto::HarnessId::Codex, 20),
            local_candidate("claude", comet_proto::HarnessId::ClaudeCode, 70),
            local_candidate("prime", comet_proto::HarnessId::PrimeAgent, 60),
            local_candidate("opencode", comet_proto::HarnessId::OpenCode, 50),
            local_candidate("cursor", comet_proto::HarnessId::Cursor, 80),
            local_candidate("mock", comet_proto::HarnessId::Mock, 90),
        ]);

        assert_eq!(
            sections
                .iter()
                .map(|section| section.harness)
                .collect::<Vec<_>>(),
            vec![
                comet_proto::HarnessId::Omp,
                comet_proto::HarnessId::ClaudeCode,
                comet_proto::HarnessId::Codex,
                comet_proto::HarnessId::PrimeAgent,
                comet_proto::HarnessId::OpenCode,
                comet_proto::HarnessId::Cursor,
                comet_proto::HarnessId::Mock,
            ]
        );
        assert_eq!(
            sections[0]
                .sessions
                .iter()
                .map(|candidate| candidate.id.as_str())
                .collect::<Vec<_>>(),
            vec!["omp-new", "omp-old"]
        );
        assert_eq!(
            sections[2]
                .sessions
                .iter()
                .map(|candidate| candidate.id.as_str())
                .collect::<Vec<_>>(),
            vec!["codex-new", "codex-old"]
        );
    }

    #[test]
    fn local_session_provider_folds_default_collapsed_and_toggle_independently() {
        let mut omp = LocalSessionProviderFold::default();
        let codex = LocalSessionProviderFold::default();

        assert!(!omp.expanded);
        assert!(!codex.expanded);
        omp.toggle(LOCAL_SESSION_PROVIDER_MAX_HEIGHT);
        assert!(omp.expanded);
        assert!(!codex.expanded);
        assert_eq!(omp.from, 0.0);
        omp.toggle(LOCAL_SESSION_PROVIDER_MAX_HEIGHT);
        assert!(!omp.expanded);
        assert_eq!(omp.from, LOCAL_SESSION_PROVIDER_MAX_HEIGHT);
        assert_eq!(omp.epoch, 2);
    }

    #[test]
    fn local_session_provider_viewports_show_four_rows_before_scrolling() {
        assert_eq!(
            LOCAL_SESSION_PROVIDER_MAX_HEIGHT,
            LOCAL_SESSION_PROVIDER_ROW_HEIGHT * 4.0
        );
        assert_eq!(
            local_session_provider_viewport_height(1),
            LOCAL_SESSION_PROVIDER_ROW_HEIGHT
        );
        assert_eq!(
            local_session_provider_viewport_height(3),
            LOCAL_SESSION_PROVIDER_ROW_HEIGHT * 3.0
        );
        assert_eq!(
            local_session_provider_viewport_height(4),
            LOCAL_SESSION_PROVIDER_MAX_HEIGHT
        );
        assert_eq!(
            local_session_provider_viewport_height(99),
            LOCAL_SESSION_PROVIDER_MAX_HEIGHT
        );
    }

    #[test]
    fn local_session_provider_wheel_distance_is_bounded_to_its_rows() {
        let last_page = gpui::ListOffset {
            item_ix: 95,
            offset_in_item: px(0.0),
        };
        let maximum = px(95.0 * LOCAL_SESSION_PROVIDER_ROW_HEIGHT);

        assert_eq!(
            local_session_provider_scroll_distance(99, gpui::ListOffset::default(), px(10_000.0),),
            maximum
        );
        assert_eq!(
            local_session_provider_scroll_distance(99, last_page, px(10_000.0)),
            px(0.0)
        );
        assert_eq!(
            local_session_provider_scroll_distance(99, last_page, px(-10_000.0)),
            -maximum
        );
    }

    #[test]
    fn imported_history_chat_requires_deterministic_id_and_no_native_session() {
        assert_eq!(
            imported_chat_history_source("local-chat-opencode-abc", None),
            Some("OpenCode")
        );
        assert_eq!(
            imported_chat_history_source("local-chat-abc", None),
            Some("Imported")
        );
        assert_eq!(
            imported_chat_history_source("local-chat-opencode-abc", Some("omp-session")),
            None
        );
        assert_eq!(imported_chat_history_source("chat-abc", None), None);
    }

    #[test]
    fn titlebar_cluster_matches_comet_window_controls() {
        // comet window-controls.tsx: `left: fullscreen ? 12 : 88` — the
        // cluster clears the {14,15} traffic lights, and reclaims the inset
        // when fullscreen hides them.
        assert_eq!(titlebar_cluster_start(false), 88.0);
        assert_eq!(titlebar_cluster_start(true), 12.0);
    }

    #[test]
    fn imported_session_without_a_space_still_shows_the_composer() {
        assert!(should_show_composer(false, true));
        assert!(should_show_composer(true, false));
        assert!(!should_show_composer(false, false));
    }

    #[test]
    fn titlebar_spacer_selects_per_platform_and_fullscreen() {
        // macOS, lights visible: spacer fills up to the 88px cluster start.
        assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
        assert_eq!(titlebar_spacer_width(true, false, 12.0), 76.0);
        assert_eq!(titlebar_spacer_width(true, false, 26.0), 62.0);
        // macOS fullscreen: the inset animates away (clamped at zero when the
        // strip's own padding already exceeds the 12px cluster start).
        assert_eq!(titlebar_spacer_width(true, true, 10.0), 2.0);
        assert_eq!(titlebar_spacer_width(true, true, 26.0), 0.0);
        // Linux / Windows: never any inset.
        assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
        assert_eq!(titlebar_spacer_width(false, true, 10.0), 0.0);
    }

    #[test]
    fn windows_caption_controls_reserve_titlebar_space() {
        assert_eq!(titlebar_right_padding(true, 16.0), 124.0);
        assert_eq!(titlebar_right_padding(false, 16.0), 16.0);
    }

    #[test]
    fn cluster_clearance_clears_the_overlay_buttons() {
        // Linux: buttons at 10..86; a 16px-padded header needs 78 more px to
        // put content at 86 + 8 breathing room.
        assert_eq!(cluster_clearance(false, false, 16.0), 78.0);
        assert_eq!(cluster_clearance(false, false, 10.0), 84.0);
        // macOS: buttons start at the 88px traffic-light cluster start.
        assert_eq!(
            cluster_clearance(true, false, 16.0),
            88.0 + 76.0 + 8.0 - 16.0
        );
        // macOS fullscreen: cluster reclaims the inset (starts at 12).
        assert_eq!(
            cluster_clearance(true, true, 16.0),
            12.0 + 76.0 + 8.0 - 16.0
        );
    }

    // ---- per-session panel flags (§1.10/1.11 parity: comet sessionPanels) ----

    #[test]
    fn session_panels_default_closed_per_chat() {
        let panels = SessionPanels::default();
        assert_eq!(panels.get("a"), ChatPanels::default());
        assert!(!panels.get("a").terminal_open);
        assert!(!panels.get("a").changes_open);
        // The new-chat canvas ("" key) is its own session, also closed.
        assert!(!panels.get("").terminal_open);
    }

    #[test]
    fn session_panels_flags_are_chat_scoped() {
        let mut panels = SessionPanels::default();
        // Opening the terminal in chat A opens it ONLY in chat A.
        assert!(panels.toggle_terminal("a"));
        assert!(panels.get("a").terminal_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("").terminal_open);
        // Changes pane in B is independent of A's terminal.
        assert!(panels.toggle_changes("b"));
        assert!(panels.get("b").changes_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("a").changes_open);
        // Switching back to A restores A's state untouched.
        assert!(panels.get("a").terminal_open);
        // Toggling off round-trips.
        assert!(!panels.toggle_terminal("a"));
        assert!(!panels.get("a").terminal_open);
    }

    #[test]
    fn session_panels_both_flags_coexist_per_chat() {
        let mut panels = SessionPanels::default();
        panels.toggle_terminal("a");
        panels.toggle_changes("a");
        assert_eq!(
            panels.get("a"),
            ChatPanels {
                terminal_open: true,
                changes_open: true
            }
        );
        assert_eq!(panels.get("b"), ChatPanels::default());
    }

    // ---- sidebar resort FLIP diff (§1.6) ----

    fn keys(list: &[(&str, f32)]) -> Vec<(String, f32)> {
        list.iter().map(|(k, h)| (k.to_string(), *h)).collect()
    }

    #[test]
    fn resort_offsets_empty_when_order_unchanged() {
        let order = keys(&[("a", 29.0), ("b", 29.0), ("c", 45.0)]);
        assert!(resort_offsets(&order, &order, 2.0).is_empty());
    }

    #[test]
    fn resort_offsets_activity_moves_row_to_top() {
        // c (bottom, y=62) jumps to top: c glides down-from-above? No — c's
        // old y is 62, new y is 0 → starts +62 below… offset = old - new = +62,
        // painted at +62 decaying to 0 (a glide UP into place). a and b shift
        // down by c's height + gap (31).
        let old = keys(&[("a", 29.0), ("b", 29.0), ("c", 29.0)]);
        let new = keys(&[("c", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        assert_eq!(offsets.get("c"), Some(&62.0));
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_respect_heights_and_gap() {
        // Tall row (45px) swaps with a short one (29px).
        let old = keys(&[("tall", 45.0), ("short", 29.0)]);
        let new = keys(&[("short", 29.0), ("tall", 45.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // short: old y 47 → new y 0; tall: old y 0 → new y 31.
        assert_eq!(offsets.get("short"), Some(&47.0));
        assert_eq!(offsets.get("tall"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_ignore_added_and_removed_keys() {
        let old = keys(&[("a", 29.0), ("gone", 29.0), ("b", 29.0)]);
        let new = keys(&[("new", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // "new" has no old position (fades in instead); "gone" just goes.
        assert!(!offsets.contains_key("new"));
        assert!(!offsets.contains_key("gone"));
        // a: old 0 → new 31 (pushed down by the insert); b: 62 → 62 (gone's
        // slot replaced by "new" of equal height — no move, no entry).
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), None);
    }

    #[test]
    fn resort_glide_spec_matches_original() {
        // §1.6: 260ms cubic-bezier(0.22, 1, 0.36, 1).
        assert_eq!(RESORT.duration_ms, 260);
        assert_eq!(RESORT.curve, motion::EASE_RESORT);
    }

    // ---- navigation history (titlebar back/forward) ----

    fn chat(id: &str) -> NavEntry {
        NavEntry::Chat(id.to_string())
    }

    #[test]
    fn nav_history_starts_with_nothing_to_walk() {
        let nav = NavHistory::new(chat(""));
        assert!(!nav.can_back());
        assert!(!nav.can_forward());
        assert_eq!(*nav.current(), chat(""));
    }

    #[test]
    fn nav_push_then_back_and_forward() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        assert!(nav.can_back());
        assert!(!nav.can_forward());

        // Back walks toward the oldest entry without dropping anything.
        assert_eq!(
            nav.back(),
            Some(chat("b")),
            "back lands on the previous route"
        );
        assert_eq!(nav.back(), Some(chat("a")));
        assert!(!nav.can_back());
        assert!(nav.can_forward());
        assert_eq!(nav.back(), None, "past the oldest entry is a no-op");

        // Forward retraces the same path.
        assert_eq!(nav.forward(), Some(chat("b")));
        assert_eq!(
            nav.forward(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert!(!nav.can_forward());
        assert_eq!(nav.forward(), None);
    }

    #[test]
    fn nav_push_dedups_the_current_route() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("a"));
        nav.push(chat("a"));
        assert_eq!(nav.len(), 1, "re-selecting the current route never stacks");
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        assert_eq!(nav.len(), 2);
    }

    #[test]
    fn nav_push_truncates_the_forward_branch() {
        // a → b → c, back to a, then push d: the b/c branch is gone (browser
        // semantics — comet's memory history PUSH truncates entries ahead).
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(chat("c"));
        nav.back();
        nav.back();
        assert_eq!(*nav.current(), chat("a"));
        assert!(nav.can_forward());
        nav.push(chat("d"));
        assert!(!nav.can_forward(), "the old branch is unreachable");
        assert_eq!(nav.len(), 2);
        assert_eq!(nav.back(), Some(chat("a")));
        assert_eq!(nav.forward(), Some(chat("d")));
    }

    #[test]
    fn nav_replace_swaps_in_place() {
        // The boot auto-select replaces the untouched canvas entry, so Back
        // stays disabled after landing in the last-used chat.
        let mut nav = NavHistory::new(chat(""));
        nav.replace(chat("boot"));
        assert_eq!(nav.len(), 1);
        assert_eq!(*nav.current(), chat("boot"));
        assert!(!nav.can_back());
    }

    #[test]
    fn nav_settings_sections_are_distinct_entries() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        nav.push(NavEntry::Settings(SettingsSection::Shortcuts));
        assert_eq!(nav.len(), 3, "section changes are navigations");
        assert_eq!(
            nav.back(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert_eq!(nav.back(), Some(chat("a")));
    }

    #[test]
    fn comment_control_falls_back_to_a_whole_message_anchor() {
        let anchor = Shell::whole_message_anchor("message-1".to_string());
        assert_eq!(anchor.target_kind, comet_proto::AnchorTargetKind::Message);
        assert_eq!(anchor.target_id, "message-1");
        assert!(anchor.exact.is_none());
        assert!(anchor.byte_range.is_none());
    }
}
