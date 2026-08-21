//! The right-pane "Changes" content (feature-inventory §1.11): a unified-diff
//! viewer driven by summary-only `WatchCheckoutDiffs` frames and an on-demand
//! `ReadCheckoutDiff` for the selected checkout.
//!
//! - pure patch parser: `diff --git` sections → file/hunk/line/notice rows,
//!   with add/delete/rename/binary detection and per-file counts;
//! - resolution: the shown diff matches the selected chat by `checkout_id`
//!   first, then by device+cwd, then cwd alone;
//! - states: *preparing* (no summary/fetched patch yet), *clean*, *list*; RPC
//!   errors show a banner while current content stays;
//! - virtualized with gpui `list()` — one row per file section; each section
//!   collapses with a 180 ms height tween (analytic heights, no measurement)
//!   and a 200 ms chevron transition;
//! - syntax highlight reuses the markdown tokenizer per diff line, computed
//!   time-sliced on the background executor and applied as paint-only run
//!   colors (layout never changes).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, ListAlignment, ListState, SharedString,
    Subscription, Task, Window, div, font, list, prelude::*, px,
};

use comet_proto::{Chat, CheckoutDiffPatch, CheckoutDiffSummary};
use comet_rpc::{ReadCheckoutDiffParams, ReadCheckoutDiffResult, methods};

use crate::markdown::highlight::{Lang, LineCarry, Token, lang_for_tag, tokenize_line};
use crate::markdown::render;
use crate::motion::{self, AnimationExt as _, CHEVRON, COLLAPSE};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub enum ChangesEvent {
    OpenAnnotations(comet_proto::SemanticAnchor),
}

impl EventEmitter<ChangesEvent> for Changes {}

// ---------------------------------------------------------------------------
// Layout numbers (analytic — they drive the fold tween)
// ---------------------------------------------------------------------------

pub const FILE_HEADER_HEIGHT: f32 = 36.0;
pub const HUNK_HEADER_HEIGHT: f32 = 28.0;
pub const DIFF_LINE_HEIGHT: f32 = 21.0;
pub const NOTICE_HEIGHT: f32 = 24.0;
pub const BODY_BOTTOM_PAD: f32 = 8.0;
/// Gutter width per line-number column.
pub const GUTTER_WIDTH: f32 = 36.0;
/// The +/−/· marker column between the gutters and the code.
pub const MARKER_WIDTH: f32 = 28.0;
/// Width of the coloured accent bar on the left edge of +/− rows.
pub const ACCENT_BAR_WIDTH: f32 = 3.0;
const DIFF_TEXT_SIZE: f32 = 12.0;

// ---------------------------------------------------------------------------
// Patch model + parser (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
    /// `\ No newline at end of file` and friends.
    Meta,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    /// Display path (the post-change side).
    pub path: String,
    /// Pre-rename path, when different.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    /// Parser-collected notices (mode changes etc.).
    pub notices: Vec<String>,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
}

impl FileDiff {
    fn new(path: String, old_path: Option<String>) -> Self {
        Self {
            path,
            old_path,
            status: FileStatus::Modified,
            binary: false,
            notices: Vec::new(),
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        }
    }
}

fn strip_git_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

/// Split the tail of a `diff --git a/… b/…` line into (old, new) paths.
/// Quoted paths (spaces/unicode) are handled; for unquoted paths with spaces
/// the split favors the last ` b/` separator, which is git's own convention.
fn parse_git_paths(rest: &str) -> (String, String) {
    fn unquote(s: &str) -> String {
        let trimmed = s.trim();
        if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
            trimmed[1..trimmed.len() - 1]
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        } else {
            trimmed.to_string()
        }
    }
    if let Some(pos) = rest.rfind(" b/").or_else(|| rest.rfind(" \"b/")) {
        let old = unquote(&rest[..pos]);
        let new = unquote(&rest[pos + 1..]);
        (
            strip_git_prefix(&old).to_string(),
            strip_git_prefix(&new).to_string(),
        )
    } else {
        let p = strip_git_prefix(&unquote(rest)).to_string();
        (p.clone(), p)
    }
}

/// Parse one `@@ -a[,b] +c[,d] @@ …` header into starting line numbers.
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@")?;
    let minus = rest.find('-')?;
    let after_minus = &rest[minus + 1..];
    let old: u32 = after_minus
        .split(|c: char| c == ',' || c.is_whitespace())
        .next()?
        .parse()
        .ok()?;
    let plus = rest.find('+')?;
    let after_plus = &rest[plus + 1..];
    let new: u32 = after_plus
        .split(|c: char| c == ',' || c.is_whitespace())
        .next()?
        .parse()
        .ok()?;
    Some((old, new))
}

/// Parse a unified git patch into file sections. Tolerant: unknown header
/// lines are skipped, truncated hunks keep what parsed so far.
pub fn parse_patch(patch: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    let mut old_no: u32 = 0;
    let mut new_no: u32 = 0;

    for raw in patch.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            let (old, new) = parse_git_paths(rest);
            let old_path = (old != new).then_some(old);
            files.push(FileDiff::new(new, old_path));
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };

        if raw.starts_with("@@") {
            if let Some((o, n)) = parse_hunk_header(raw) {
                old_no = o;
                new_no = n;
                file.hunks.push(Hunk {
                    header: raw.to_string(),
                    lines: Vec::new(),
                });
                in_hunk = true;
            }
            continue;
        }

        if in_hunk {
            let mut chars = raw.chars();
            let marker = chars.next();
            let body: String = chars.collect();
            let line = match marker {
                Some('+') => {
                    file.additions += 1;
                    let l = DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(new_no),
                        text: body,
                    };
                    new_no += 1;
                    Some(l)
                }
                Some('-') => {
                    file.deletions += 1;
                    let l = DiffLine {
                        kind: LineKind::Del,
                        old_no: Some(old_no),
                        new_no: None,
                        text: body,
                    };
                    old_no += 1;
                    Some(l)
                }
                Some(' ') | None => {
                    let l = DiffLine {
                        kind: LineKind::Context,
                        old_no: Some(old_no),
                        new_no: Some(new_no),
                        text: body,
                    };
                    old_no += 1;
                    new_no += 1;
                    Some(l)
                }
                Some('\\') => Some(DiffLine {
                    kind: LineKind::Meta,
                    old_no: None,
                    new_no: None,
                    text: raw.trim_start_matches('\\').trim().to_string(),
                }),
                _ => {
                    // A non-hunk line ends the hunk; reprocess as a header.
                    in_hunk = false;
                    None
                }
            };
            if let Some(line) = line
                && let Some(hunk) = file.hunks.last_mut()
            {
                hunk.lines.push(line);
                continue;
            }
            if in_hunk {
                continue;
            }
        }

        // File header territory.
        if raw.starts_with("new file mode") {
            file.status = FileStatus::Added;
        } else if raw.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
        } else if let Some(from) = raw.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(from.trim().to_string());
        } else if let Some(to) = raw.strip_prefix("rename to ") {
            file.status = FileStatus::Renamed;
            file.path = to.trim().to_string();
        } else if raw.starts_with("Binary files") || raw.starts_with("GIT binary patch") {
            file.binary = true;
        } else if let Some(mode) = raw.strip_prefix("new mode ") {
            file.notices
                .push(format!("Mode changed to {}", mode.trim()));
        } else if let Some(new) = raw.strip_prefix("+++ ") {
            let new = new.trim();
            if new == "/dev/null" {
                file.status = FileStatus::Deleted;
            } else if file.old_path.is_none() {
                file.path = strip_git_prefix(new).to_string();
            }
        } else if let Some(old) = raw.strip_prefix("--- ")
            && old.trim() == "/dev/null"
        {
            file.status = FileStatus::Added;
        }
        // "index …", "similarity index …", "old mode …" etc.: skipped.
    }
    files
}

/// Derived per-file notice rows (new/deleted/renamed/binary + parser notices).
pub fn file_notices(file: &FileDiff) -> Vec<String> {
    let mut notices = Vec::new();
    match file.status {
        FileStatus::Added => notices.push("New file".to_string()),
        FileStatus::Deleted => notices.push("Deleted file".to_string()),
        FileStatus::Renamed => {
            let from = file.old_path.as_deref().unwrap_or("?");
            notices.push(format!("Renamed from {from}"));
        }
        FileStatus::Modified => {}
    }
    if file.binary {
        notices.push("Binary file — contents not shown".to_string());
    }
    notices.extend(file.notices.iter().cloned());
    notices
}

/// Analytic expanded-body height — drives the 180 ms fold tween without
/// measurement.
pub fn body_height(file: &FileDiff) -> f32 {
    let notices = file_notices(file).len() as f32 * NOTICE_HEIGHT;
    let hunks = file.hunks.len() as f32 * HUNK_HEADER_HEIGHT;
    let lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    notices + hunks + lines as f32 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
}

// ---------------------------------------------------------------------------
// Resolution + states (pure)
// ---------------------------------------------------------------------------

/// The diff summary shown for a chat: `checkout_id` match first, then device+cwd,
/// then cwd alone (§1.11).
pub fn resolve_diff<'a>(
    diffs: &'a [CheckoutDiffSummary],
    chat: &Chat,
) -> Option<&'a CheckoutDiffSummary> {
    if let Some(checkout_id) = chat.checkout_id.as_deref()
        && let Some(diff) = diffs.iter().find(|d| d.checkout_id == checkout_id)
    {
        return Some(diff);
    }
    let cwd = chat.cwd.as_deref()?;
    diffs
        .iter()
        .find(|d| d.device_id == chat.device_id && d.cwd == cwd)
        .or_else(|| diffs.iter().find(|d| d.cwd == cwd))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffPhase {
    /// No diff for this checkout yet.
    Preparing,
    /// Diff arrived and it's empty — working tree clean.
    Clean,
    List,
}

/// Compact worktree state for shell-level status surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangesSummary {
    pub file_count: usize,
    pub additions: u32,
    pub deletions: u32,
    pub truncated: bool,
}

pub fn diff_phase(resolved: Option<&CheckoutDiffSummary>) -> DiffPhase {
    match resolved {
        None => DiffPhase::Preparing,
        Some(diff) if diff.files.is_empty() => DiffPhase::Clean,
        Some(_) => DiffPhase::List,
    }
}

/// Header label: "N Uncommitted change(s)".
pub fn uncommitted_label(count: usize) -> String {
    if count == 1 {
        "1 Uncommitted change".to_string()
    } else {
        format!("{count} Uncommitted changes")
    }
}

/// Fold a summary-only `WatchCheckoutDiffs` frame into the current set. The
/// engine sends the complete latest list, so every frame replaces wholesale.
pub fn apply_diff_frame(diffs: &mut Vec<CheckoutDiffSummary>, value: serde_json::Value) -> bool {
    match serde_json::from_value::<Vec<CheckoutDiffSummary>>(value) {
        Ok(all) if *diffs != all => {
            *diffs = all;
            true
        }
        Ok(_) => false,
        Err(err) => {
            tracing::warn!(error = %err, "changes: dropping malformed diff summary frame");
            false
        }
    }
}

/// Language for a file path's extension (drives per-line highlighting).
pub fn lang_for_path(path: &str) -> Option<Lang> {
    let ext = path.rsplit('/').next()?.rsplit('.').next()?;
    lang_for_tag(ext)
}

fn hash64(parts: &[&str]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    for p in parts {
        p.hash(&mut hasher);
    }
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedPatch {
    checkout_id: String,
    checksum: String,
    patch: String,
}

impl SelectedPatch {
    fn matches(&self, diff: &CheckoutDiffSummary) -> bool {
        self.checkout_id == diff.checkout_id && self.checksum == diff.checksum
    }
}

/// Install a reply only if it still belongs to the selected summary. This is
/// deliberately pure so stale-response behavior is directly testable.
fn cache_fetched_patch(
    cached: &mut Option<SelectedPatch>,
    selected: &CheckoutDiffSummary,
    fetched: CheckoutDiffPatch,
) -> bool {
    if fetched.checkout_id != selected.checkout_id || fetched.checksum != selected.checksum {
        return false;
    }
    *cached = Some(SelectedPatch {
        checkout_id: fetched.checkout_id,
        checksum: fetched.checksum,
        patch: fetched.patch,
    });
    true
}

struct ParsedDiff {
    /// `checkout_id:checksum` — identity of the parsed content.
    key: String,
    truncated: bool,
    additions: u32,
    deletions: u32,
    file_count: usize,
    files: Arc<Vec<FileDiff>>,
}

#[derive(Default, Clone, Copy)]
struct FileFold {
    collapsed: bool,
    /// Bumped per toggle — keys the height tween + chevron transition.
    epoch: usize,
    from: f32,
    to: f32,
    /// When the toggle happened: the tweens are armed only briefly after the
    /// click — gpui replays an element's animation on remount, and in the
    /// virtualized list a row scrolling back into view is a remount (the
    /// transcript's tool groups had the same flash; user report).
    toggled_at: Option<std::time::Instant>,
}

/// Tween arming window after a fold toggle (COLLAPSE's 180ms plus margin).
const FOLD_TWEEN_WINDOW: Duration = Duration::from_millis(400);

impl FileFold {
    fn animating(&self) -> bool {
        self.epoch > 0
            && self
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW)
    }
}

struct HighlightSlot {
    fingerprint: u64,
    lines: Option<Arc<Vec<Vec<Token>>>>,
    _task: Option<Task<()>>,
}

async fn yield_now() {
    let mut yielded = false;
    futures::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}

/// The Changes pane entity. Lazy: no RPC until [`Changes::ensure_watch`] runs
/// (the shell calls it when the pane first opens).
pub struct Changes {
    state: Entity<AppState>,
    /// Summary-only latest state for every checkout on the watched device.
    diffs: Vec<CheckoutDiffSummary>,
    /// The sole retained patch: exactly the selected checkout/checksum.
    selected_patch: Option<SelectedPatch>,
    fetching_key: Option<String>,
    fetch_task: Option<Task<()>>,
    started: bool,
    error: Option<SharedString>,
    /// Device the running watch targets: `None` = the connected engine itself,
    /// `Some(id)` = a remote chat's host (relay-forwarded). The stream only
    /// carries the TARGET device's checkouts, so a selection change onto a
    /// chat hosted elsewhere tears the watch down and re-subscribes.
    watch_target: Option<String>,
    watch_task: Option<Task<()>>,
    parsed: Option<ParsedDiff>,
    parsing_key: Option<String>,
    parse_task: Option<Task<()>>,
    folds: HashMap<String, FileFold>,
    highlights: HashMap<String, HighlightSlot>,
    list: ListState,
    _observe: Subscription,
}

impl Changes {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        Self {
            state,
            diffs: Vec::new(),
            selected_patch: None,
            fetching_key: None,
            fetch_task: None,
            started: false,
            error: None,
            watch_target: None,
            watch_task: None,
            parsed: None,
            parsing_key: None,
            parse_task: None,
            folds: HashMap::new(),
            highlights: HashMap::new(),
            list: ListState::new(0, ListAlignment::Top, px(320.0)),
            _observe: observe,
        }
    }

    /// The selected chat's host device when it differs from the connected
    /// engine's own — diffs are produced where the checkout lives, so a
    /// remote chat's watch must relay-forward (`targetDeviceId`) to its host.
    /// Without this the local stream simply never carries the remote checkout
    /// and the pane sits on "Preparing diff…" forever (user report).
    fn desired_target(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let device = state.selected_chat_row()?.device_id.clone();
        (state.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
    }

    /// Start the `WatchCheckoutDiffs` subscription (idempotent per target).
    /// Retries with a flat 2 s delay if the stream fails or ends; the last
    /// content stays visible under an error banner meanwhile.
    pub fn ensure_watch(&mut self, cx: &mut Context<Self>) {
        let target = self.desired_target(cx);
        if self.started && self.watch_target == target {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            // Engine still booting — retry on the next state change via sync().
            return;
        };
        // Retarget: the old tasks and stream drop; rows or a cached patch from
        // the previous device must never resolve against the new target.
        if self.started {
            self.diffs.clear();
            self.clear_selected_content();
            self.error = None;
        }
        self.started = true;
        self.watch_target = target.clone();
        self.watch_task = Some(Self::spawn_watch(engine, target, cx));
    }

    fn spawn_watch(
        engine: EngineHandle,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let mut params = serde_json::Map::new();
                if let Some(target) = &target {
                    params.insert(
                        "targetDeviceId".into(),
                        serde_json::Value::String(target.clone()),
                    );
                }
                let subscribed = engine
                    .client()
                    .subscribe(
                        methods::WATCH_CHECKOUT_DIFFS,
                        serde_json::Value::Object(params),
                    )
                    .await;
                match subscribed {
                    Ok(mut rx) => {
                        while let Some(value) = rx.recv().await {
                            let alive = this.update(cx, |changes, cx| {
                                let cleared_error = changes.error.take().is_some();
                                let changed = apply_diff_frame(&mut changes.diffs, value);
                                if changed {
                                    changes.sync(cx);
                                }
                                if changed || cleared_error {
                                    cx.notify();
                                }
                            });
                            if alive.is_err() {
                                return;
                            }
                        }
                        // Stream ended (engine restart / reconnect): banner + retry.
                        if this
                            .update(cx, |changes, cx| {
                                changes.error = Some("Diff stream interrupted — retrying".into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(err) => {
                        if this
                            .update(cx, |changes, cx| {
                                changes.error =
                                    Some(format!("Diff watch unavailable: {err}").into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
    }

    fn resolved(&self, cx: &App) -> Option<&CheckoutDiffSummary> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        resolve_diff(&self.diffs, chat)
    }

    /// Current selected checkout summary without exposing or reparsing the patch.
    pub fn summary(&self, cx: &App) -> Option<ChangesSummary> {
        self.resolved(cx).map(|diff| ChangesSummary {
            file_count: diff.files.len(),
            additions: diff.additions,
            deletions: diff.deletions,
            truncated: diff.truncated,
        })
    }

    fn clear_selected_content(&mut self) {
        self.selected_patch = None;
        self.fetching_key = None;
        self.fetch_task = None;
        self.parsed = None;
        self.parsing_key = None;
        self.parse_task = None;
        self.list.reset(0);
        self.folds.clear();
        self.highlights.clear();
    }

    /// Reconcile the selected summary, its one-entry patch cache, and parsed
    /// content. Selection/checksum changes drop the old patch before fetching.
    fn sync(&mut self, cx: &mut Context<Self>) {
        self.ensure_watch(cx);
        let Some(diff) = self.resolved(cx).cloned() else {
            if self.selected_patch.is_some()
                || self.parsed.is_some()
                || self.fetching_key.is_some()
                || self.parsing_key.is_some()
            {
                self.clear_selected_content();
                cx.notify();
            }
            return;
        };
        let key = format!("{}:{}", diff.checkout_id, diff.checksum);

        if !self
            .selected_patch
            .as_ref()
            .is_some_and(|patch| patch.matches(&diff))
        {
            self.selected_patch = None;
            if self.fetching_key.as_deref() != Some(key.as_str()) {
                self.fetch_task = None;
                self.fetching_key = None;
            }
        }
        if self.parsed.as_ref().is_some_and(|parsed| parsed.key != key) {
            self.parsed = None;
            self.parsing_key = None;
            self.parse_task = None;
            self.list.reset(0);
            self.folds.clear();
            self.highlights.clear();
            cx.notify();
        }

        if self.selected_patch.is_none() {
            if self.fetching_key.as_deref() != Some(key.as_str()) {
                self.fetch_selected(diff, key, cx);
            }
            return;
        }
        if self.parsed.as_ref().is_some_and(|parsed| parsed.key == key)
            || self.parsing_key.as_deref() == Some(key.as_str())
        {
            return;
        }

        let patch = self
            .selected_patch
            .as_ref()
            .expect("selected patch checked above")
            .patch
            .clone();
        let truncated = diff.truncated;
        let additions = diff.additions;
        let deletions = diff.deletions;
        let file_count = diff.files.len();
        self.parsing_key = Some(key.clone());
        self.parse_task = Some(cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move { parse_patch(&patch) })
                .await;
            this.update(cx, |changes, cx| {
                let current = changes
                    .resolved(cx)
                    .map(|current| format!("{}:{}", current.checkout_id, current.checksum));
                if current.as_deref() != Some(key.as_str())
                    || !changes.selected_patch.as_ref().is_some_and(|patch| {
                        patch.checkout_id == diff.checkout_id && patch.checksum == diff.checksum
                    })
                {
                    return;
                }
                changes.parsing_key = None;
                changes.list.reset(files.len());
                changes.folds.clear();
                changes.highlights.clear();
                changes.parsed = Some(ParsedDiff {
                    key,
                    truncated,
                    additions,
                    deletions,
                    file_count: if file_count > 0 {
                        file_count
                    } else {
                        files.len()
                    },
                    files: Arc::new(files),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    fn fetch_selected(&mut self, diff: CheckoutDiffSummary, key: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = serde_json::to_value(ReadCheckoutDiffParams {
            checkout_id: diff.checkout_id.clone(),
            checksum: diff.checksum.clone(),
            target_device_id: self.watch_target.clone(),
        })
        .expect("diff read params serialize");
        self.fetching_key = Some(key.clone());
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            loop {
                match engine
                    .client()
                    .call_as::<ReadCheckoutDiffResult>(methods::READ_CHECKOUT_DIFF, params.clone())
                    .await
                {
                    Ok(ReadCheckoutDiffResult {
                        diff: Some(fetched),
                    }) => {
                        this.update(cx, |changes, cx| {
                            if changes.fetching_key.as_deref() != Some(key.as_str()) {
                                return;
                            }
                            changes.fetching_key = None;
                            let Some(current) = changes.resolved(cx).cloned() else {
                                return;
                            };
                            if cache_fetched_patch(&mut changes.selected_patch, &current, fetched) {
                                changes.error = None;
                                changes.sync(cx);
                                cx.notify();
                            } else {
                                // A mismatched reply never enters the cache. Retry
                                // the still-current exact key rather than leaving
                                // the selected pane stuck on its loading state.
                                changes.sync(cx);
                            }
                        })
                        .ok();
                        return;
                    }
                    Ok(ReadCheckoutDiffResult { diff: None }) => {
                        let active = this
                            .update(cx, |changes, _| {
                                changes.fetching_key.as_deref() == Some(key.as_str())
                            })
                            .unwrap_or(false);
                        if !active {
                            return;
                        }
                        // The checksum raced a newer engine snapshot. Let its
                        // summary arrive, but retry in case the checkout returns
                        // to this exact checksum without producing a new UI frame.
                        cx.background_executor().timer(Duration::from_secs(2)).await;
                    }
                    Err(err) => {
                        let active = this
                            .update(cx, |changes, cx| {
                                let active = changes.fetching_key.as_deref() == Some(key.as_str());
                                if active {
                                    changes.error =
                                        Some(format!("Diff unavailable: {err} — retrying").into());
                                    cx.notify();
                                }
                                active
                            })
                            .unwrap_or(false);
                        if !active {
                            return;
                        }
                        cx.background_executor().timer(Duration::from_secs(2)).await;
                    }
                }
            }
        }));
    }

    fn toggle_fold(&mut self, path: &str, expanded_height: f32) {
        let fold = self.folds.entry(path.to_string()).or_default();
        let currently_collapsed = fold.collapsed;
        fold.from = if currently_collapsed {
            0.0
        } else {
            expanded_height
        };
        fold.to = if currently_collapsed {
            expanded_height
        } else {
            0.0
        };
        fold.collapsed = !currently_collapsed;
        fold.epoch += 1;
        fold.toggled_at = Some(std::time::Instant::now());
    }

    /// Tokens for a file's diff lines (paint-only). Kicks a time-sliced
    /// background tokenize when missing; returns the current best.
    fn request_highlight(
        &mut self,
        file: &FileDiff,
        parsed_key: &str,
        cx: &mut Context<Self>,
    ) -> Option<Arc<Vec<Vec<Token>>>> {
        let lang = lang_for_path(&file.path)?;
        let fingerprint = hash64(&[parsed_key, &file.path]);
        if let Some(slot) = self.highlights.get(&file.path)
            && slot.fingerprint == fingerprint
        {
            return slot.lines.clone();
        }
        let texts: Vec<(LineKind, String)> = file
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter().map(|l| (l.kind, l.text.clone())))
            .collect();
        let path = file.path.clone();
        let task = cx.spawn(async move |this, cx| {
            let lines = cx
                .background_executor()
                .spawn(async move {
                    let mut out = Vec::with_capacity(texts.len());
                    for (ix, (kind, text)) in texts.iter().enumerate() {
                        // Diff lines are fragments — no carry across lines.
                        let tokens = match kind {
                            LineKind::Meta => Vec::new(),
                            _ => tokenize_line(lang, text, LineCarry::None).0,
                        };
                        out.push(tokens);
                        if ix % 128 == 127 {
                            yield_now().await;
                        }
                    }
                    out
                })
                .await;
            this.update(cx, |changes, cx| {
                if let Some(slot) = changes.highlights.get_mut(&path)
                    && slot.fingerprint == fingerprint
                {
                    slot.lines = Some(Arc::new(lines));
                    cx.notify();
                }
            })
            .ok();
        });
        self.highlights.insert(
            file.path.clone(),
            HighlightSlot {
                fingerprint,
                lines: None,
                _task: Some(task),
            },
        );
        None
    }

    // ---- rendering ----

    fn annotation_anchor_for_file(
        &self,
        file: &FileDiff,
        exact: Option<String>,
        cx: &App,
    ) -> Option<comet_proto::SemanticAnchor> {
        let state = self.state.read(cx);
        if let Some(anchor) = state.collaboration.as_ref().and_then(|snapshot| {
            snapshot.publications.iter().rev().find_map(|publication| {
                let comet_proto::PublicationValue::Annotation(annotation) = &publication.value
                else {
                    return None;
                };
                let matches = match annotation.anchor.file.as_ref() {
                    Some(comet_proto::FileTargetReference::LocalWorkspacePath {
                        relative_path,
                        ..
                    }) => relative_path == &file.path,
                    Some(comet_proto::FileTargetReference::ScaffoldArtifact {
                        artifact_id,
                        artifact_uri,
                        ..
                    }) => {
                        artifact_id == &file.path
                            || artifact_uri.as_deref() == Some(file.path.as_str())
                    }
                    None => false,
                };
                matches.then(|| annotation.anchor.clone())
            })
        }) {
            return Some(anchor);
        }
        let chat = state.selected_chat_row()?;
        let workspace_id = chat
            .checkout_id
            .clone()
            .or_else(|| chat.space_id.clone())
            .unwrap_or_else(|| chat.device_id.clone());
        Some(crate::multiplayer::local_file_anchor(
            workspace_id,
            file.path.clone(),
            exact,
        ))
    }

    fn render_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(parsed) = &self.parsed else {
            return gpui::Empty.into_any_element();
        };
        let files = parsed.files.clone();
        let parsed_key = parsed.key.clone();
        let Some(file) = files.get(ix) else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let expanded_height = body_height(file);
        let fold = self.folds.get(&file.path).copied().unwrap_or_default();
        let highlight = self.request_highlight(file, &parsed_key, cx);
        let path = file.path.clone();

        let annotation_anchor = self.annotation_anchor_for_file(file, None, cx);
        let header = self.render_file_header(ix, file, &fold, expanded_height, &theme, cx);
        let body = render_file_body(file, highlight, annotation_anchor, &theme, cx);
        // Collapse: 180 ms committed-height tween on toggle (windowed — see
        // FileFold::animating); steady states paint at the target height
        // directly.
        let body: AnyElement = if fold.animating() {
            let (from, to) = (fold.from, fold.to);
            div()
                .overflow_hidden()
                .child(body)
                .with_animation(
                    SharedString::from(format!("fold-{path}-{}", fold.epoch)),
                    COLLAPSE.animation(),
                    move |el, t| el.h(px(motion::lerp(from, to, t))),
                )
                .into_any_element()
        } else {
            let target = if fold.collapsed { 0.0 } else { expanded_height };
            div()
                .overflow_hidden()
                .h(px(target))
                .child(body)
                .into_any_element()
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(crate::theme::hairline(0.04))
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_file_header(
        &mut self,
        ix: usize,
        file: &FileDiff,
        fold: &FileFold,
        expanded_height: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = fold.collapsed;
        let path = file.path.clone();
        let adds = file.additions;
        let dels = file.deletions;
        let annotation_anchor = self.annotation_anchor_for_file(file, None, cx);
        let has_notes = annotation_anchor.as_ref().is_some_and(|anchor| {
            self.state
                .read(cx)
                .collaboration
                .as_ref()
                .is_some_and(|snapshot| {
                    snapshot.publications.iter().any(|publication| {
                        matches!(
                            &publication.value,
                            comet_proto::PublicationValue::Annotation(annotation)
                                if annotation.anchor.target_id == anchor.target_id
                        )
                    })
                })
        });
        let path_for_fold = path.clone();

        // Chevron (comet checkout-diff-sidebar): chevron-right closed,
        // chevron-down open; gpui divs have no rotation transform at the
        // pinned rev, so the glyph swap crossfades over the same 200 ms.
        let chevron_icon = if collapsed {
            crate::icons::ALT_ARROW_RIGHT
        } else {
            crate::icons::ALT_ARROW_DOWN
        };
        let chevron = div().flex_none().size(px(14.0)).child(
            crate::icons::icon(chevron_icon)
                .size(px(13.0))
                .text_color(theme.text_muted.opacity(0.7)),
        );
        let chevron: AnyElement = if fold.animating() {
            chevron
                .with_animation(
                    SharedString::from(format!("chev-{path}-{}", fold.epoch)),
                    CHEVRON.animation(),
                    |el, t| el.opacity(0.25 + 0.75 * t),
                )
                .into_any_element()
        } else {
            chevron.into_any_element()
        };

        // Header row: chevron + mono path (one quiet tone) + right-aligned
        // +N / −N counts on a slightly raised wash.
        div()
            .id(SharedString::from(format!("file-hdr-{ix}")))
            .h(px(FILE_HEADER_HEIGHT))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(Theme::SPACE_MD))
            .bg(crate::theme::ink(0.025))
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::ink(0.05)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_fold(&path_for_fold, expanded_height);
                cx.notify();
            }))
            .child(chevron)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(12.0))
                    .text_color(theme.text_dim)
                    .child(SharedString::from(file.path.clone())),
            )
            .when(file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("BIN")),
                )
            })
            .when_some(annotation_anchor, |el, anchor| {
                el.child(
                    div()
                        .id(SharedString::from(format!("file-notes-{ix}")))
                        .flex_none()
                        .px(px(Theme::SPACE_SM))
                        .py(px(2.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .text_size(px(10.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.surface_raised_hover))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(ChangesEvent::OpenAnnotations(anchor.clone()));
                        }))
                        .child(SharedString::from(if has_notes { "Notes" } else { "Note" })),
                )
            })
            .when(adds > 0 || !file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(add_color(theme))
                        .child(SharedString::from(format!("+{adds}"))),
                )
            })
            .when(dels > 0 || !file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(del_color(theme))
                        .child(SharedString::from(format!("−{dels}"))),
                )
            })
            .into_any_element()
    }

    fn render_header_strip(&self, theme: &Theme) -> Option<AnyElement> {
        let parsed = self.parsed.as_ref()?;
        Some(
            div()
                .flex_none()
                .h(px(36.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .px(px(Theme::SPACE_LG))
                .border_b_1()
                .border_color(crate::theme::hairline(0.06))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(uncommitted_label(parsed.file_count))),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(add_color(theme))
                        .child(SharedString::from(format!("+{}", parsed.additions))),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(del_color(theme))
                        .child(SharedString::from(format!("−{}", parsed.deletions))),
                )
                .child(div().flex_1())
                .when(parsed.truncated, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(theme.warning.opacity(0.08))
                            .text_color(theme.warning.opacity(0.75))
                            .child(SharedString::from("Partial snapshot")),
                    )
                })
                .into_any_element(),
        )
    }
}

/// Green for additions — sampled from the reference diff (soft emerald).
fn add_color(theme: &Theme) -> gpui::Hsla {
    theme.diff_add // emerald-400
}

/// Red for deletions — softer than the theme danger, per the reference diff.
fn del_color(theme: &Theme) -> gpui::Hsla {
    theme.diff_del // red-400
}

/// Diff syntax palette — since round 9 the transcript's code blocks share the
/// same soft hues, so this simply delegates to [`render::token_color`].
fn diff_token_color(class: crate::markdown::highlight::TokenClass, theme: &Theme) -> gpui::Hsla {
    render::token_color(class, theme)
}

/// The expanded body of one file section: notices, hunk headers, +/-/context
/// lines with a coloured accent bar, dual line-number gutters, a marker
/// column, and paint-only syntax runs (comet checkout-diff-sidebar).
fn render_file_body(
    file: &FileDiff,
    highlight: Option<Arc<Vec<Vec<Token>>>>,
    annotation_anchor: Option<comet_proto::SemanticAnchor>,
    theme: &Theme,
    cx: &mut Context<Changes>,
) -> AnyElement {
    let mono = font(theme.font_mono.clone());
    let mut line_ix = 0usize;
    let mut children: Vec<AnyElement> = Vec::new();

    for notice in file_notices(file) {
        children.push(
            div()
                .h(px(NOTICE_HEIGHT))
                .flex_none()
                .flex()
                .items_center()
                .px(px(Theme::SPACE_LG))
                .text_size(px(11.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(notice))
                .into_any_element(),
        );
    }

    let mut add_bg = add_color(theme);
    add_bg.a = 0.055;
    let mut del_bg = del_color(theme);
    del_bg.a = 0.055;
    let hunk_bg = theme.diff_hunk_bg;

    for (hunk_ix, hunk) in file.hunks.iter().enumerate() {
        let hunk_anchor = annotation_anchor.clone().map(|mut anchor| {
            anchor.exact = Some(crate::multiplayer::bounded_anchor_exact(&hunk.header));
            anchor.byte_range = None;
            anchor.prefix_hash = None;
            anchor.suffix_hash = None;
            anchor
        });
        children.push(
            div()
                .id(SharedString::from(format!(
                    "diff-section-{}-{hunk_ix}",
                    file.path
                )))
                .h(px(HUNK_HEADER_HEIGHT))
                .flex_none()
                .flex()
                .items_center()
                .px(px(Theme::SPACE_LG))
                .bg(hunk_bg)
                .font_family(theme.font_mono.clone())
                .text_size(px(11.0))
                .text_color(theme.text_faint)
                .when_some(hunk_anchor, |el, anchor| {
                    el.cursor_pointer()
                        .hover(|style| style.bg(theme.surface_raised_hover))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(ChangesEvent::OpenAnnotations(anchor.clone()));
                        }))
                })
                .child(SharedString::from(hunk.header.clone()))
                .into_any_element(),
        );
        for line in &hunk.lines {
            let tokens = highlight
                .as_ref()
                .and_then(|lines| lines.get(line_ix))
                .map(|tokens| tokens.as_slice())
                .unwrap_or(&[]);
            line_ix += 1;

            if line.kind == LineKind::Meta {
                children.push(
                    div()
                        .h(px(DIFF_LINE_HEIGHT))
                        .flex_none()
                        .flex()
                        .items_center()
                        .pl(px(ACCENT_BAR_WIDTH
                            + 2.0 * GUTTER_WIDTH
                            + MARKER_WIDTH
                            + 12.0))
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .italic()
                        .child(SharedString::from(line.text.clone()))
                        .into_any_element(),
                );
                continue;
            }

            let (marker, marker_color, row_bg, accent, number_color) = match line.kind {
                LineKind::Add => (
                    "+",
                    add_color(theme),
                    Some(add_bg),
                    Some(add_color(theme).opacity(0.55)),
                    add_color(theme).opacity(0.9),
                ),
                LineKind::Del => (
                    "−",
                    del_color(theme),
                    Some(del_bg),
                    Some(del_color(theme).opacity(0.55)),
                    del_color(theme).opacity(0.9),
                ),
                _ => (
                    "·",
                    theme.text_faint.opacity(0.5),
                    None,
                    None,
                    theme.text_faint.opacity(0.8),
                ),
            };
            let gutter = |no: Option<u32>, color: gpui::Hsla| {
                div()
                    .w(px(GUTTER_WIDTH))
                    .flex_none()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.0))
                    .text_color(color)
                    .flex()
                    .justify_end()
                    .pr(px(8.0))
                    .child(SharedString::from(
                        no.map(|number| number.to_string()).unwrap_or_default(),
                    ))
            };
            let runs = render::runs_with_palette(
                &line.text,
                tokens,
                &mono,
                theme.text.opacity(0.92),
                |class| diff_token_color(class, theme),
            );
            let line_anchor = annotation_anchor.clone().map(|mut anchor| {
                anchor.exact = Some(crate::multiplayer::bounded_anchor_exact(&line.text));
                anchor.byte_range = None;
                anchor.prefix_hash = None;
                anchor.suffix_hash = None;
                anchor
            });
            let marker_id = format!(
                "diff-note-{}-{}",
                file.path,
                line.new_no.or(line.old_no).unwrap_or(line_ix as u32)
            );
            children.push(
                div()
                    .h(px(DIFF_LINE_HEIGHT))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .when_some(row_bg, |el, bg| el.bg(bg))
                    .child(
                        div()
                            .w(px(ACCENT_BAR_WIDTH))
                            .h_full()
                            .flex_none()
                            .when_some(accent, |el, color| el.bg(color)),
                    )
                    .child(gutter(
                        line.old_no,
                        if line.kind == LineKind::Del {
                            number_color
                        } else {
                            theme.text_faint.opacity(0.8)
                        },
                    ))
                    .child(gutter(
                        line.new_no,
                        if line.kind == LineKind::Add {
                            number_color
                        } else {
                            theme.text_faint.opacity(0.8)
                        },
                    ))
                    .child(
                        div()
                            .id(SharedString::from(marker_id))
                            .w(px(MARKER_WIDTH))
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(DIFF_TEXT_SIZE))
                            .text_color(marker_color)
                            .font_family(theme.font_mono.clone())
                            .when_some(line_anchor, |el, anchor| {
                                el.cursor_pointer()
                                    .hover(|style| style.bg(theme.surface_raised_hover))
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(ChangesEvent::OpenAnnotations(anchor.clone()));
                                    }))
                            })
                            .child(SharedString::from(marker)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .pl(px(12.0))
                            .font_family(theme.font_mono.clone())
                            .text_size(px(DIFF_TEXT_SIZE))
                            .whitespace_nowrap()
                            .child(gpui::StyledText::new(line.text.clone()).with_runs(runs)),
                    )
                    .into_any_element(),
            );
        }
    }

    div()
        .flex()
        .flex_col()
        .pb(px(BODY_BOTTOM_PAD))
        .children(children)
        .into_any_element()
}

impl Render for Changes {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let resolved = self.resolved(cx);
        // With no session selected (new-chat canvas) there is nothing to
        // prepare — show the quiet empty state, not an endless spinner.
        let phase = if self.state.read(cx).selected_chat_row().is_none() {
            DiffPhase::Clean
        } else {
            diff_phase(resolved)
        };
        let error = self.error.clone();

        let content: AnyElement = match phase {
            DiffPhase::Preparing => div()
                .flex_1()
                .p(px(Theme::SPACE_MD))
                .flex()
                .flex_col()
                .gap(px(Theme::SPACE_SM))
                .children((0..4).map(|index| {
                    div()
                        .h(px(if index == 0 { 36.0 } else { 64.0 }))
                        .w_full()
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .bg(theme.element_hover)
                        .opacity(0.45)
                }))
                .into_any_element(),
            DiffPhase::Clean => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No uncommitted changes"))
                .into_any_element(),
            DiffPhase::List => {
                if self.parsed.is_some() {
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .children(self.render_header_strip(&theme))
                        .child(
                            list(self.list.clone(), cx.processor(Self::render_row))
                                .flex_1()
                                .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
                        )
                        .into_any_element()
                } else {
                    // Diff known, parse still running. Keep its eventual
                    // file-row geometry stable instead of centering a spinner.
                    div()
                        .flex_1()
                        .p(px(Theme::SPACE_MD))
                        .flex()
                        .flex_col()
                        .gap(px(Theme::SPACE_SM))
                        .children((0..3).map(|_| {
                            div()
                                .h(px(64.0))
                                .w_full()
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .bg(theme.element_hover)
                                .opacity(0.45)
                        }))
                        .into_any_element()
                }
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .flex_none()
                        .px(px(Theme::SPACE_MD))
                        .py(px(4.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .text_size(px(11.0))
                        .text_color(theme.warning)
                        .child(message),
                )
            })
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    const PATCH: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 111..222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,5 @@ fn main
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    let x = 1;
 }
@@ -10,2 +11,2 @@
 // tail
-old_line
+new_line
diff --git a/added.txt b/added.txt
new file mode 100644
--- /dev/null
+++ b/added.txt
@@ -0,0 +1,2 @@
+first
+second
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
diff --git a/img.png b/img.png
new file mode 100644
Binary files /dev/null and b/img.png differ
diff --git a/old_name.rs b/new_name.rs
similarity index 90%
rename from old_name.rs
rename to new_name.rs
";

    #[test]
    fn parses_files_hunks_and_lines() {
        let files = parse_patch(PATCH);
        assert_eq!(files.len(), 5);

        let main = &files[0];
        assert_eq!(main.path, "src/main.rs");
        assert_eq!(main.status, FileStatus::Modified);
        assert_eq!(main.hunks.len(), 2);
        assert_eq!(main.additions, 3);
        assert_eq!(main.deletions, 2);
        let h0 = &main.hunks[0];
        assert_eq!(h0.header, "@@ -1,4 +1,5 @@ fn main");
        assert_eq!(h0.lines.len(), 5);
        assert_eq!(h0.lines[0].kind, LineKind::Context);
        assert_eq!(h0.lines[0].old_no, Some(1));
        assert_eq!(h0.lines[0].new_no, Some(1));
        assert_eq!(h0.lines[1].kind, LineKind::Del);
        assert_eq!(h0.lines[1].old_no, Some(2));
        assert_eq!(h0.lines[1].new_no, None);
        assert_eq!(h0.lines[2].kind, LineKind::Add);
        assert_eq!(h0.lines[2].new_no, Some(2));
        assert_eq!(h0.lines[3].kind, LineKind::Add);
        assert_eq!(h0.lines[3].new_no, Some(3));
        // Closing context line: numbering advanced past the add/del block.
        assert_eq!(h0.lines[4].old_no, Some(3));
        assert_eq!(h0.lines[4].new_no, Some(4));
        // Second hunk restarts numbering from its header.
        assert_eq!(main.hunks[1].lines[0].old_no, Some(10));
        assert_eq!(main.hunks[1].lines[0].new_no, Some(11));
    }

    #[test]
    fn detects_new_deleted_binary_and_renamed() {
        let files = parse_patch(PATCH);
        let added = &files[1];
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!(added.additions, 2);
        // The no-newline marker rides as a Meta line.
        let last = added.hunks[0].lines.last().unwrap();
        assert_eq!(last.kind, LineKind::Meta);
        assert!(last.text.contains("No newline"));
        assert!(file_notices(added).iter().any(|n| n == "New file"));

        let deleted = &files[2];
        assert_eq!(deleted.status, FileStatus::Deleted);
        assert_eq!(deleted.deletions, 1);
        assert!(file_notices(deleted).iter().any(|n| n == "Deleted file"));

        let binary = &files[3];
        assert!(binary.binary);
        assert_eq!(binary.status, FileStatus::Added);
        assert!(binary.hunks.is_empty());
        assert!(file_notices(binary).iter().any(|n| n.contains("Binary")));

        let renamed = &files[4];
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.path, "new_name.rs");
        assert_eq!(renamed.old_path.as_deref(), Some("old_name.rs"));
        assert!(
            file_notices(renamed)
                .iter()
                .any(|n| n.contains("old_name.rs"))
        );
    }

    #[test]
    fn empty_and_garbage_patches_parse_to_nothing() {
        assert!(parse_patch("").is_empty());
        assert!(parse_patch("not a diff\nat all\n").is_empty());
        // Truncated mid-hunk: keeps what parsed.
        let files = parse_patch("diff --git a/x b/x\n@@ -1,9 +1,9 @@\n ctx\n+add");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        assert_eq!(files[0].additions, 1);
    }

    #[test]
    fn quoted_and_spaced_paths() {
        let (old, new) = parse_git_paths("a/simple.rs b/simple.rs");
        assert_eq!((old.as_str(), new.as_str()), ("simple.rs", "simple.rs"));
        let (old, new) = parse_git_paths("\"a/with space.rs\" \"b/with space.rs\"");
        assert_eq!(old, "with space.rs");
        assert_eq!(new, "with space.rs");
    }

    #[test]
    fn hunk_headers_parse_with_and_without_counts() {
        assert_eq!(parse_hunk_header("@@ -1,4 +2,5 @@"), Some((1, 2)));
        assert_eq!(parse_hunk_header("@@ -7 +9 @@ fn ctx"), Some((7, 9)));
        assert_eq!(parse_hunk_header("@@ garbage"), None);
    }

    #[test]
    fn body_height_is_analytic() {
        let files = parse_patch(PATCH);
        let main = &files[0];
        let lines: usize = main.hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(
            body_height(main),
            2.0 * HUNK_HEADER_HEIGHT + lines as f32 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
        );
        // Notices add height (added file: 1 notice + meta line inside hunk).
        let added = &files[1];
        assert_eq!(
            body_height(added),
            NOTICE_HEIGHT + HUNK_HEADER_HEIGHT + 3.0 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
        );
    }

    fn diff(checkout: &str, device: &str, cwd: &str, checksum: &str) -> CheckoutDiffSummary {
        CheckoutDiffSummary {
            checkout_id: checkout.into(),
            device_id: device.into(),
            cwd: cwd.into(),
            files: Vec::new(),
            additions: 0,
            deletions: 0,
            truncated: false,
            checksum: checksum.into(),
            updated_at: Utc::now(),
        }
    }

    fn chat(checkout: Option<&str>, device: &str, cwd: Option<&str>) -> Chat {
        Chat {
            id: "c1".into(),
            device_id: device.into(),
            title: None,
            archived: false,
            cwd: cwd.map(Into::into),
            branch: None,
            checkout_id: checkout.map(Into::into),
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
        }
    }

    #[test]
    fn diff_resolution_prefers_checkout_id_then_cwd() {
        let diffs = vec![
            diff("co-1", "dev-a", "/repo/one", "x"),
            diff("co-2", "dev-b", "/repo/two", "y"),
        ];
        // checkout_id match wins even when cwd points elsewhere.
        let c = chat(Some("co-2"), "dev-a", Some("/repo/one"));
        assert_eq!(resolve_diff(&diffs, &c).unwrap().checkout_id, "co-2");
        // Unknown checkout falls back to device+cwd.
        let c = chat(Some("co-9"), "dev-a", Some("/repo/one"));
        assert_eq!(resolve_diff(&diffs, &c).unwrap().checkout_id, "co-1");
        // Wrong device still matches by cwd alone.
        let c = chat(None, "dev-z", Some("/repo/two"));
        assert_eq!(resolve_diff(&diffs, &c).unwrap().checkout_id, "co-2");
        // Nothing to go on.
        let c = chat(None, "dev-a", None);
        assert!(resolve_diff(&diffs, &c).is_none());
        let c = chat(None, "dev-a", Some("/elsewhere"));
        assert!(resolve_diff(&diffs, &c).is_none());
    }

    #[test]
    fn phases_come_from_summary_without_patch_bytes() {
        assert_eq!(diff_phase(None), DiffPhase::Preparing);
        let clean = diff("co", "d", "/w", "clean-sum");
        assert_eq!(diff_phase(Some(&clean)), DiffPhase::Clean);
        let mut changed = diff("co", "d", "/w", "changed-sum");
        changed.files.push(comet_proto::DiffFileSummary {
            path: "x".into(),
            old_path: None,
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            binary: false,
        });
        assert_eq!(diff_phase(Some(&changed)), DiffPhase::List);
    }

    #[test]
    fn header_label_pluralizes() {
        assert_eq!(uncommitted_label(0), "0 Uncommitted changes");
        assert_eq!(uncommitted_label(1), "1 Uncommitted change");
        assert_eq!(uncommitted_label(4), "4 Uncommitted changes");
    }

    #[test]
    fn summary_frames_replace_wholesale_and_contain_no_patch() {
        let mut diffs = Vec::new();
        let one = diff("co-1", "d", "/w", "sum-1");
        let frame = serde_json::to_value(vec![one.clone()]).unwrap();
        assert!(frame[0].get("patch").is_none());
        assert!(apply_diff_frame(&mut diffs, frame));
        assert_eq!(diffs, vec![one.clone()]);

        assert!(!apply_diff_frame(
            &mut diffs,
            serde_json::to_value(vec![one]).unwrap()
        ));
        let two = diff("co-2", "d", "/x", "sum-2");
        assert!(apply_diff_frame(
            &mut diffs,
            serde_json::to_value(vec![two.clone()]).unwrap()
        ));
        assert_eq!(diffs, vec![two]);

        // A legacy single/full-patch-shaped payload is malformed, not retained.
        assert!(!apply_diff_frame(
            &mut diffs,
            serde_json::json!({"checkoutId": "co-3", "patch": "large bytes"})
        ));
        assert_eq!(diffs[0].checkout_id, "co-2");
    }

    #[test]
    fn selected_patch_cache_ignores_stale_replies() {
        let current = diff("co-1", "d", "/w", "sum-new");
        let mut cached = Some(SelectedPatch {
            checkout_id: "co-1".into(),
            checksum: "sum-new".into(),
            patch: "new patch".into(),
        });
        let stale = CheckoutDiffPatch {
            checkout_id: "co-1".into(),
            checksum: "sum-old".into(),
            patch: "old patch".into(),
        };
        assert!(!cache_fetched_patch(&mut cached, &current, stale));
        assert_eq!(cached.as_ref().unwrap().patch, "new patch");

        let fetched = CheckoutDiffPatch {
            checkout_id: "co-1".into(),
            checksum: "sum-new".into(),
            patch: "exact patch".into(),
        };
        assert!(cache_fetched_patch(&mut cached, &current, fetched));
        assert_eq!(cached.as_ref().unwrap().patch, "exact patch");
    }

    #[test]
    fn langs_resolve_from_paths() {
        assert_eq!(lang_for_path("src/main.rs"), Some(Lang::Rust));
        assert_eq!(lang_for_path("a/b/app.tsx"), Some(Lang::Js));
        assert_eq!(lang_for_path("Cargo.toml"), Some(Lang::Toml));
        assert_eq!(lang_for_path("script.sh"), Some(Lang::Bash));
        assert_eq!(lang_for_path("README"), None);
        assert_eq!(lang_for_path("img.png"), None);
    }
}
