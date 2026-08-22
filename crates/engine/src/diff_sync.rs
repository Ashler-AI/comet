//! CheckoutDiffSync — checkout-scoped working-tree diff production (feature-inventory
//! §3.5; port of comet's `checkout-diff-sync.ts` + `git-metadata-sync.ts`).
//!
//! Chats do not own working-tree state: a concrete Git checkout does. This service
//! groups this device's chats by their canonical checkout identity (`chat.cwd` →
//! [`Repos::checkout_identity`]), computes one bounded atomic snapshot per checkout,
//! and publishes it three ways:
//!
//! - the local `WatchCheckoutDiffs` stream (a watch channel of every checkout's
//!   latest [`CheckoutDiffSummary`], without patch bytes);
//! - a [`DiffSidecar`] JSON `POST {edge}/diff/{chatId}` for every syncing chat of
//!   the checkout (bearer = engine edge token), so "review pending changes while
//!   the host sleeps" works;
//! - `chat.branch` upkeep: the same fs events cover the checkout's git dir (HEAD),
//!   so each snapshot reconciles mismatched workspace chat rows' `branch` (and
//!   `checkoutId` at reconcile time).
//!
//! Fast recursive `notify` watchers (debounced [`WATCH_DEBOUNCE`]) are backed by a
//! slow 2-minute repair tick because native watchers may coalesce or drop events.
//! Snapshots carry a sha256 checksum; an unchanged checksum publishes nothing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};

use comet_proto::{Chat, CheckoutDiffPatch, CheckoutDiffSummary, DiffFileSummary};

use crate::EngineError;
use crate::doc_host::EdgeConfig;
use crate::repos::{CheckoutIdentity, Repos};
use crate::workspace_host::WorkspaceHost;

/// Hard cap on the unified patch (plus untracked hunks) — "Partial snapshot".
pub const MAX_PATCH_BYTES: usize = 3 * 1024 * 1024;
/// Trailing debounce after a filesystem event burst.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);
/// Slow repair pass: re-sync every checkout in one bounded global queue.
const REPAIR_INTERVAL: Duration = Duration::from_secs(120);
/// Max subdirectories a checkout may have before we skip its live recursive
/// watch (one OS watch per dir; past this the watcher thread's own bookkeeping
/// costs more than instant diffs are worth). A normal source tree is well
/// under this; a node_modules/vendored tree blows past it. The repair tick
/// still covers skipped checkouts.
const MAX_WATCH_DIRS: usize = 8_000;
/// Git snapshots are I/O-heavy and each starts several subprocesses. A single
/// global permit prevents many active worktrees from launching them in parallel.
const MAX_CONCURRENT_CAPTURES: usize = 1;
/// `git hash-object -t tree /dev/null` — diff base for repos with no commits yet.
const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Latest-only diff sidecar published to each chat's session DO slot
/// (`POST /diff/{chatId}`; shape: edge/src/session-doc/sidecar.ts).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSidecar {
    pub chat_id: String,
    pub device_id: String,
    pub checkout_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub patch: String,
    pub files: Vec<DiffFileSummary>,
    pub additions: u32,
    pub deletions: u32,
    pub truncated: bool,
    /// Epoch millis.
    pub published_at: i64,
}

/// One bounded atomic snapshot of a checkout's working tree.
#[derive(Debug, Clone)]
pub struct DiffSnapshot {
    pub branch: String,
    pub head_sha: Option<String>,
    pub patch: String,
    pub files: Vec<DiffFileSummary>,
    pub additions: u32,
    pub deletions: u32,
    pub truncated: bool,
    pub checksum: String,
}

struct CheckoutEntry {
    identity: CheckoutIdentity,
    chats: Mutex<Vec<Chat>>,
    /// Latest bounded snapshot. Kept engine-side so the patch can be read for
    /// one exact checksum without placing patch bytes in the watch channel.
    latest: Mutex<Option<Arc<DiffSnapshot>>>,
    kick_tx: mpsc::UnboundedSender<()>,
    /// Keeps the recursive fs watchers alive; dropped on entry close.
    _watchers: Vec<notify::RecommendedWatcher>,
}

struct DiffSyncInner {
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    edge: Option<EdgeConfig>,
    http: reqwest::Client,
    entries: Mutex<HashMap<String, Arc<CheckoutEntry>>>,
    /// Exact cwd → canonical checkout identity. Workspace publications often
    /// touch chat metadata without moving the checkout; do not rerun rev-parse.
    identity_cache: Mutex<HashMap<PathBuf, CheckoutIdentity>>,
    capture_permits: tokio::sync::Semaphore,
    diffs_tx: watch::Sender<Vec<CheckoutDiffSummary>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct CheckoutDiffSync {
    inner: Arc<DiffSyncInner>,
}

impl CheckoutDiffSync {
    /// Build and start the sync loop: follows the workspace chat watch and runs the
    /// 2-minute repair tick. Requires a tokio runtime.
    pub fn start(
        repos: Repos,
        workspace: WorkspaceHost,
        device_id: &str,
        edge: Option<EdgeConfig>,
    ) -> Self {
        let (diffs_tx, _) = watch::channel(Vec::new());
        let sync = Self {
            inner: Arc::new(DiffSyncInner {
                repos,
                workspace: workspace.clone(),
                device_id: device_id.to_string(),
                edge,
                http: reqwest::Client::new(),
                entries: Mutex::new(HashMap::new()),
                identity_cache: Mutex::new(HashMap::new()),
                capture_permits: tokio::sync::Semaphore::new(MAX_CONCURRENT_CAPTURES),
                diffs_tx,
            }),
        };
        tokio::spawn(diff_sync_task(
            Arc::downgrade(&sync.inner),
            workspace.watch_chats(),
        ));
        sync
    }

    /// `WatchCheckoutDiffs` source: every tracked checkout's latest summary.
    pub fn watch_diffs(&self) -> watch::Receiver<Vec<CheckoutDiffSummary>> {
        self.inner.diffs_tx.subscribe()
    }

    /// Return the bounded patch only when `checksum` still names the latest
    /// snapshot for `checkout_id`. A raced/stale request returns `None`.
    pub fn read_diff(&self, checkout_id: &str, checksum: &str) -> Option<CheckoutDiffPatch> {
        let entries = lock(&self.inner.entries);
        let entry = entries.get(checkout_id)?;
        let latest = lock(&entry.latest);
        let snapshot = latest
            .as_ref()
            .filter(|snapshot| snapshot.checksum == checksum)?;
        Some(CheckoutDiffPatch {
            checkout_id: checkout_id.to_string(),
            checksum: checksum.to_string(),
            patch: snapshot.patch.clone(),
        })
    }

    /// Regroup this device's chats by checkout identity, then (re)build watchers.
    /// Public for tests (the background task calls it on every chat change).
    pub async fn reconcile_now(&self) {
        let chats = self.inner.workspace.watch_chats().borrow().clone();
        reconcile(&self.inner, chats).await;
    }

    /// Kick an immediate sync of every tracked checkout (repair-tick path).
    pub fn sync_all(&self) {
        for entry in lock(&self.inner.entries).values() {
            let _ = entry.kick_tx.send(());
        }
    }
}

// ---------------------------------------------------------------------------
// Reconcile: chats ⇄ checkout entries
// ---------------------------------------------------------------------------

async fn reconcile(inner: &Arc<DiffSyncInner>, chats: Vec<Chat>) {
    // Archived chats have no live surface, so they must not retain filesystem
    // watchers or periodic diff capture. Resolve each cwd once: historical
    // chats commonly share a checkout, and running git once per chat made
    // startup work grow with the entire conversation history.
    let mut chats_by_cwd: HashMap<PathBuf, Vec<Chat>> = HashMap::new();
    for chat in chats {
        if chat.device_id != inner.device_id || chat.archived {
            continue;
        }
        let Some(cwd) = chat.cwd.as_deref() else {
            continue;
        };
        chats_by_cwd
            .entry(PathBuf::from(cwd))
            .or_default()
            .push(chat);
    }

    let mut groups: HashMap<String, (CheckoutIdentity, Vec<Chat>)> = HashMap::new();
    for (cwd, chats) in chats_by_cwd {
        // Cache exact cwd resolutions only. Prefix reuse would misclassify a
        // nested repository or submodule as its parent checkout.
        let cached = lock(&inner.identity_cache).get(&cwd).cloned();
        let identity = match cached {
            Some(identity) => identity,
            None => match inner.repos.checkout_identity(&cwd).await {
                Ok(identity) => {
                    let mut cache = lock(&inner.identity_cache);
                    cache.insert(cwd.clone(), identity.clone());
                    cache.insert(identity.root.clone(), identity.clone());
                    identity
                }
                Err(err) => {
                    tracing::debug!(cwd = %cwd.display(), error = %err, "diff-sync: not a checkout");
                    continue;
                }
            },
        };
        for chat in &chats {
            // Stamp the row's checkoutId so every device groups this chat correctly.
            if chat.checkout_id.as_deref() != Some(identity.id.as_str())
                && let Err(err) = inner.workspace.set_chat_checkout(&chat.id, &identity.id)
            {
                tracing::debug!(chat = %chat.id, error = %err, "diff-sync: checkoutId write failed");
            }
        }
        groups
            .entry(identity.id.clone())
            .or_insert_with(|| (identity, Vec::new()))
            .1
            .extend(chats);
    }

    // Close entries whose checkout no longer has chats; drop their published diff.
    let removed: Vec<String> = {
        let mut entries = lock(&inner.entries);
        let removed: Vec<String> = entries
            .keys()
            .filter(|id| !groups.contains_key(*id))
            .cloned()
            .collect();
        for id in &removed {
            entries.remove(id); // dropping the entry drops watchers + ends its task
        }
        removed
    };
    if !removed.is_empty() {
        lock(&inner.identity_cache).retain(|_, identity| !removed.contains(&identity.id));
    }
    if !removed.is_empty() {
        publish_watch(inner);
    }

    // Update surviving entries; add new ones (initial sync kicked on add).
    for (checkout_id, (identity, chats)) in groups {
        let existing = lock(&inner.entries).get(&checkout_id).cloned();
        match existing {
            Some(entry) => {
                let has_new = {
                    let mut held = lock(&entry.chats);
                    let previous: HashSet<String> = held.iter().map(|c| c.id.clone()).collect();
                    let has_new = chats.iter().any(|c| !previous.contains(&c.id));
                    *held = chats;
                    has_new
                };
                if has_new {
                    let _ = entry.kick_tx.send(()); // new chat needs a sidecar now
                }
            }
            None => add_entry(inner, identity, chats).await,
        }
    }
}

/// True if `root`'s directory tree exceeds [`MAX_WATCH_DIRS`] — the signal that
/// a live recursive watch would cost more than it's worth. Bounded BFS: stops
/// the moment the budget is blown (never walks a whole node_modules), skips
/// symlinks (a symlinked dep cycle must not send this into a spin), and treats
/// unreadable dirs as leaves. `.git` internal churn is real diff signal, so it
/// counts toward the budget rather than being skipped.
fn exceeds_watch_budget(root: &Path) -> bool {
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut seen = 0usize;
    while let Some(dir) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // `file_type()` on the dirent does NOT follow symlinks — a symlinked
            // directory reports as a symlink and is skipped, so cyclic deps
            // (pnpm/npm) can't blow up the walk.
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                seen += 1;
                if seen > MAX_WATCH_DIRS {
                    return true;
                }
                queue.push_back(entry.path());
            }
        }
    }
    false
}

async fn add_entry(inner: &Arc<DiffSyncInner>, identity: CheckoutIdentity, chats: Vec<Chat>) {
    let (kick_tx, kick_rx) = mpsc::unbounded_channel();
    let watcher_identity = identity.clone();
    let watcher_kick_tx = kick_tx.clone();
    let watchers = match tokio::task::spawn_blocking(move || {
        build_watchers(&watcher_identity, &watcher_kick_tx)
    })
    .await
    {
        Ok(watchers) => watchers,
        Err(err) => {
            tracing::warn!(checkout = %identity.root.display(), error = %err,
                "diff-sync: watcher setup task failed");
            Vec::new()
        }
    };

    let entry = Arc::new(CheckoutEntry {
        identity,
        chats: Mutex::new(chats),
        latest: Mutex::new(None),
        kick_tx: kick_tx.clone(),
        _watchers: watchers,
    });
    lock(&inner.entries).insert(entry.identity.id.clone(), entry.clone());
    tokio::spawn(entry_task(
        Arc::downgrade(inner),
        Arc::downgrade(&entry),
        kick_rx,
    ));
    let _ = kick_tx.send(()); // initial snapshot
}

fn build_ignore_matcher(root: &Path) -> ignore::gitignore::Gitignore {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    let gitignore = root.join(".gitignore");
    if gitignore.is_file() {
        builder.add(gitignore);
    }
    builder.build().unwrap_or_else(|err| {
        tracing::debug!(error = %err, "diff-sync: ignore matcher build failed");
        ignore::gitignore::GitignoreBuilder::new(root)
            .build()
            .expect("empty ignore matcher")
    })
}

fn is_diff_signal(
    identity: &CheckoutIdentity,
    ignored: &ignore::gitignore::Gitignore,
    path: &Path,
) -> bool {
    if let Ok(relative) = path.strip_prefix(&identity.git_dir) {
        return relative.as_os_str().is_empty()
            || relative.components().next().is_some_and(|component| {
                matches!(
                    component.as_os_str().to_str(),
                    Some("HEAD" | "index" | "refs" | "packed-refs")
                )
            });
    }
    let Ok(relative) = path.strip_prefix(&identity.root) else {
        return true;
    };
    !ignored
        .matched_path_or_any_parents(relative, path.is_dir())
        .is_ignore()
}

fn event_needs_capture(
    identity: &CheckoutIdentity,
    ignored: &ignore::gitignore::Gitignore,
    event: &notify::Event,
) -> bool {
    event.paths.is_empty()
        || event
            .paths
            .iter()
            .any(|path| is_diff_signal(identity, ignored, path))
}

/// Build native filesystem watchers away from the async runtime. Both the
/// bounded directory walk and notify's recursive registration perform blocking
/// filesystem I/O; doing either on a Tokio worker delayed auth, IPC, and UI
/// readiness for minutes on machines with many historical worktrees.
fn build_watchers(
    identity: &CheckoutIdentity,
    kick_tx: &mpsc::UnboundedSender<()>,
) -> Vec<notify::RecommendedWatcher> {
    let mut watchers = Vec::new();
    let mut targets: Vec<&PathBuf> = vec![&identity.root];
    if !identity.git_dir.starts_with(&identity.root) {
        targets.push(&identity.git_dir);
    }
    let ignored = Arc::new(build_ignore_matcher(&identity.root));
    for target in targets {
        // A recursive `notify` watch installs one OS watch per subdirectory and
        // has no way to prune subtrees. On a checkout carrying big dependency
        // trees (node_modules, vendored deps) that is tens of thousands of
        // watches: the watcher thread pegs a core just maintaining them — even
        // with the tree completely idle. If the tree blows the budget, skip the
        // live watch; the 2-minute repair tick still keeps the diff correct.
        if exceeds_watch_budget(target) {
            tracing::info!(path = %target.display(),
                "diff-sync: tree too large to watch live; relying on the repair tick");
            continue;
        }
        let tx = kick_tx.clone();
        let filter_identity = identity.clone();
        let ignored = ignored.clone();
        let watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                if event
                    .as_ref()
                    .is_ok_and(|event| event_needs_capture(&filter_identity, &ignored, event))
                {
                    let _ = tx.send(());
                }
            });
        match watcher {
            Ok(mut watcher) => {
                use notify::Watcher as _;
                match watcher.watch(target, notify::RecursiveMode::Recursive) {
                    Ok(()) => watchers.push(watcher),
                    Err(err) => {
                        tracing::debug!(path = %target.display(), error = %err, "diff-sync: watch failed")
                    }
                }
            }
            Err(err) => tracing::debug!(error = %err, "diff-sync: watcher create failed"),
        }
    }
    watchers
}

/// Per-checkout task: trailing-debounce fs kicks, then compute + publish. Runs
/// syncs sequentially — kicks during a sync accumulate and trigger another pass.
async fn entry_task(
    inner: Weak<DiffSyncInner>,
    entry: Weak<CheckoutEntry>,
    mut kick_rx: mpsc::UnboundedReceiver<()>,
) {
    while kick_rx.recv().await.is_some() {
        // Trailing debounce: wait for the burst to settle.
        loop {
            match tokio::time::timeout(WATCH_DEBOUNCE, kick_rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return, // entry closed mid-burst
                Err(_) => break,
            }
        }
        let (Some(inner), Some(entry)) = (inner.upgrade(), entry.upgrade()) else {
            return;
        };
        sync_entry(&inner, &entry).await;
    }
}

// ---------------------------------------------------------------------------
// Snapshot + publish
// ---------------------------------------------------------------------------

async fn sync_entry(inner: &Arc<DiffSyncInner>, entry: &Arc<CheckoutEntry>) {
    // Capture the expected metadata before the async git read. A title-driven
    // branch rename may complete while capture is in flight.
    let chats = lock(&entry.chats).clone();
    let snapshot = {
        let Ok(_permit) = inner.capture_permits.acquire().await else {
            return;
        };
        match capture_diff(&inner.repos, &entry.identity.root).await {
            Ok(snapshot) => Arc::new(snapshot),
            Err(err) => {
                tracing::debug!(checkout = %entry.identity.root.display(), error = %err,
                    "diff-sync: capture failed");
                return;
            }
        }
    };

    // chat.branch upkeep — the git-dir watcher covers HEAD, so every snapshot
    // reconciles mismatched rows (repair tick covers dropped events). The CAS
    // prevents this capture from overwriting a newer title-driven branch.
    for chat in &chats {
        if chat.branch.as_deref() == Some(snapshot.branch.as_str()) {
            continue;
        }
        if let Err(err) = inner.workspace.compare_and_set_chat_branch(
            &chat.id,
            chat.branch.as_deref(),
            &snapshot.branch,
        ) {
            tracing::debug!(chat = %chat.id, error = %err, "diff-sync: branch write failed");
        }
    }

    {
        let entries = lock(&inner.entries);
        if !entries.contains_key(&entry.identity.id) {
            return; // closed while computing
        }
    }
    if lock(&entry.latest)
        .as_ref()
        .is_some_and(|latest| latest.checksum == snapshot.checksum)
    {
        return; // unchanged — publish nothing
    }
    *lock(&entry.latest) = Some(snapshot.clone());

    let diff = CheckoutDiffSummary {
        checkout_id: entry.identity.id.clone(),
        device_id: inner.device_id.clone(),
        cwd: entry.identity.root.to_string_lossy().to_string(),
        files: snapshot.files.clone(),
        additions: snapshot.additions,
        deletions: snapshot.deletions,
        truncated: snapshot.truncated,
        checksum: snapshot.checksum.clone(),
        updated_at: chrono::Utc::now(),
    };
    publish_watch_with(inner, Some(diff));

    // Latest-only sidecar to every syncing chat's session DO slot.
    if let Some(edge) = &inner.edge {
        for chat in &chats {
            let sidecar = DiffSidecar {
                chat_id: chat.id.clone(),
                device_id: inner.device_id.clone(),
                checkout_path: entry.identity.root.to_string_lossy().to_string(),
                branch: Some(snapshot.branch.clone()),
                head_sha: snapshot.head_sha.clone(),
                patch: snapshot.patch.clone(),
                files: snapshot.files.clone(),
                additions: snapshot.additions,
                deletions: snapshot.deletions,
                truncated: snapshot.truncated,
                published_at: chrono::Utc::now().timestamp_millis(),
            };
            let url = format!("{}/diff/{}", edge.url.trim_end_matches('/'), chat.id);
            // Fresh bearer per request — never the boot-time snapshot.
            let Some(bearer) = edge.bearer().await else {
                tracing::debug!(chat = %chat.id, "diff-sync: sidecar skipped (signed out)");
                continue;
            };
            let result = inner
                .http
                .post(&url)
                .bearer_auth(&bearer)
                .json(&sidecar)
                .send()
                .await;
            match result {
                Ok(response) if !response.status().is_success() => {
                    tracing::debug!(chat = %chat.id, status = %response.status(),
                        "diff-sync: sidecar publish rejected");
                }
                Err(err) => {
                    tracing::debug!(chat = %chat.id, error = %err, "diff-sync: sidecar publish failed");
                }
                Ok(_) => {}
            }
        }
    }
}

/// Re-emit the watch channel from the current entries' cached summaries,
/// replacing (or inserting) `updated`.
fn publish_watch_with(inner: &Arc<DiffSyncInner>, updated: Option<CheckoutDiffSummary>) {
    let live: HashSet<String> = lock(&inner.entries).keys().cloned().collect();
    inner.diffs_tx.send_modify(|diffs| {
        diffs.retain(|d| live.contains(&d.checkout_id));
        if let Some(updated) = updated
            && live.contains(&updated.checkout_id)
        {
            match diffs
                .iter_mut()
                .find(|d| d.checkout_id == updated.checkout_id)
            {
                Some(slot) => *slot = updated,
                None => diffs.push(updated),
            }
        }
        diffs.sort_by(|a, b| a.checkout_id.cmp(&b.checkout_id));
    });
}

fn publish_watch(inner: &Arc<DiffSyncInner>) {
    publish_watch_with(inner, None);
}

/// Chat-watch follower + repair tick. Holds only weak handles so dropping the
/// service tears the loop down.
async fn diff_sync_task(inner: Weak<DiffSyncInner>, mut chats_rx: watch::Receiver<Vec<Chat>>) {
    let mut repair = tokio::time::interval(REPAIR_INTERVAL);
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    repair.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            changed = chats_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(inner) = inner.upgrade() else { break };
                let chats = chats_rx.borrow_and_update().clone();
                reconcile(&inner, chats).await;
            }
            _ = repair.tick() => {
                let Some(inner) = inner.upgrade() else { break };
                let chats = chats_rx.borrow().clone();
                reconcile(&inner, chats).await;
                for entry in lock(&inner.entries).values() {
                    let _ = entry.kick_tx.send(());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Diff capture (exposed for tests)
// ---------------------------------------------------------------------------

struct Capture {
    stdout: Vec<u8>,
    truncated: bool,
}

/// Run git capturing stdout under a hard byte ceiling — the child is killed once
/// the cap is hit, so an arbitrarily large repository diff never buffers fully.
async fn capture_git(cwd: &Path, args: &[&str], max_bytes: usize) -> Result<Capture, EngineError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(cwd).args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| EngineError::Other(format!("git spawn failed: {e}")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| EngineError::Other("git stdout unavailable".into()))?;
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        let n = stdout
            .read(&mut buf)
            .await
            .map_err(|e| EngineError::Other(format!("git read failed: {e}")))?;
        if n == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(out.len());
        if n > remaining {
            out.extend_from_slice(&buf[..remaining]);
            truncated = true;
            let _ = child.start_kill();
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| EngineError::Other(format!("git wait failed: {e}")))?;
    if !output.status.success() && !truncated {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        return Err(EngineError::Other(if message.is_empty() {
            format!("git exited {}", output.status)
        } else {
            format!("git: {message}")
        }));
    }
    Ok(Capture {
        stdout: out,
        truncated,
    })
}

fn split_z(value: &[u8]) -> Vec<String> {
    value
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect()
}

fn status_code(xy: &str) -> char {
    xy.chars().find(|code| *code != '.').unwrap_or('M')
}

fn file_summary(path: String, old_path: Option<String>, code: char) -> DiffFileSummary {
    let status = match code {
        'A' | '?' => "added",
        'D' => "deleted",
        'R' => "renamed",
        'C' => "copied",
        'U' => "unmerged",
        _ => "modified",
    };
    DiffFileSummary {
        path,
        old_path,
        status: status.to_string(),
        additions: 0,
        deletions: 0,
        binary: false,
    }
}

fn parse_status_v2(value: &[u8]) -> (String, String, Vec<DiffFileSummary>, Vec<String>) {
    let records = split_z(value);
    let mut branch = "HEAD".to_string();
    let mut head = String::new();
    let mut files = Vec::new();
    let mut untracked = Vec::new();
    let mut i = 0usize;
    while i < records.len() {
        let record = &records[i];
        i += 1;
        if let Some(value) = record.strip_prefix("# branch.oid ") {
            if value != "(initial)" {
                head = value.to_string();
            }
            continue;
        }
        if let Some(value) = record.strip_prefix("# branch.head ") {
            if value != "(detached)" {
                branch = value.to_string();
            }
            continue;
        }
        if record.starts_with("1 ") {
            let fields: Vec<&str> = record.splitn(9, ' ').collect();
            if let (Some(xy), Some(path)) = (fields.get(1), fields.get(8)) {
                files.push(file_summary((*path).to_string(), None, status_code(xy)));
            }
            continue;
        }
        if record.starts_with("2 ") {
            let fields: Vec<&str> = record.splitn(10, ' ').collect();
            let old_path = records.get(i).cloned();
            i += usize::from(old_path.is_some());
            if let (Some(xy), Some(path)) = (fields.get(1), fields.get(9)) {
                files.push(file_summary((*path).to_string(), old_path, status_code(xy)));
            }
            continue;
        }
        if record.starts_with("u ") {
            let fields: Vec<&str> = record.splitn(11, ' ').collect();
            if let Some(path) = fields.get(10) {
                files.push(file_summary((*path).to_string(), None, 'U'));
            }
            continue;
        }
        if let Some(path) = record.strip_prefix("? ") {
            untracked.push(path.to_string());
        }
    }
    (branch, head, files, untracked)
}

fn split_numstat_patch(value: &[u8]) -> (&[u8], &[u8]) {
    const MARKER: &[u8] = b"\0\0diff --git ";
    value
        .windows(MARKER.len())
        .position(|window| window == MARKER)
        .map_or((value, &[]), |at| (&value[..at], &value[at + 2..]))
}

fn apply_numstat(files: &mut [DiffFileSummary], value: &[u8]) {
    // With -z, a rename record is `adds<TAB>dels<TAB><NUL>old<NUL>new<NUL>`.
    let records: Vec<String> = value
        .split(|b| *b == 0)
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect();
    let mut i = 0usize;
    while i < records.len() {
        let record = &records[i];
        if record.is_empty() {
            i += 1;
            continue;
        }
        let mut parts = record.splitn(3, '\t');
        let adds = parts.next().unwrap_or_default().to_string();
        let dels = parts.next().unwrap_or_default().to_string();
        let inline_path = parts.next().unwrap_or_default().to_string();
        let path = if inline_path.is_empty() {
            // Rename: the next two records are old, new.
            let new_path = records.get(i + 2).cloned().unwrap_or_default();
            i += 2;
            new_path
        } else {
            inline_path
        };
        i += 1;
        if let Some(file) = files.iter_mut().find(|f| f.path == path) {
            file.additions = adds.parse().unwrap_or(0);
            file.deletions = dels.parse().unwrap_or(0);
            file.binary = adds == "-" || dels == "-";
        }
    }
}

fn quote_patch_path(path: &str) -> String {
    if path
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\')
    {
        serde_json::to_string(path).unwrap_or_else(|_| format!("\"{path}\""))
    } else {
        path.to_string()
    }
}

/// Synthesize a new-file hunk for an untracked file (git diff never shows them).
fn untracked_patch(path: &str, content: &str) -> String {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let body: String = lines
        .iter()
        .map(|line| format!("+{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let a = quote_patch_path(&format!("a/{path}"));
    let b = quote_patch_path(&format!("b/{path}"));
    format!(
        "diff --git {a} {b}\nnew file mode 100644\n--- /dev/null\n+++ {b}\n@@ -0,0 +1,{} @@\n{body}\n",
        lines.len()
    )
}

/// One bounded atomic snapshot: one porcelain-v2 status supplies HEAD, branch,
/// tracked state, and untracked paths; one combined numstat+patch diff supplies
/// line counts and patch bytes. The previous six-command sequence amplified
/// every filesystem burst across every checkout.
pub async fn capture_diff(_repos: &Repos, root: &Path) -> Result<DiffSnapshot, EngineError> {
    let status = capture_git(
        root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
            "-z",
        ],
        2 * 1024 * 1024,
    )
    .await?;
    let (branch, head, mut files, mut untracked) = parse_status_v2(&status.stdout);
    let base: &str = if head.is_empty() {
        EMPTY_TREE_SHA
    } else {
        &head
    };
    let tracked = capture_git(
        root,
        &[
            "diff",
            "--numstat",
            "--patch",
            "-z",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "--unified=3",
            base,
            "--",
        ],
        MAX_PATCH_BYTES + 2 * 1024 * 1024,
    )
    .await?;
    let (numstat, patch_bytes) = split_numstat_patch(&tracked.stdout);
    apply_numstat(&mut files, numstat);

    let patch_truncated = patch_bytes.len() > MAX_PATCH_BYTES;
    let mut patch =
        String::from_utf8_lossy(&patch_bytes[..patch_bytes.len().min(MAX_PATCH_BYTES)]).to_string();
    let mut truncated = tracked.truncated || patch_truncated || status.truncated;
    if tracked.truncated || patch_truncated {
        let boundary = patch.rfind('\n').unwrap_or(0);
        patch.truncate(boundary);
        patch.push_str("\n# Crew diff truncated\n");
    }

    untracked.sort();
    for path in untracked {
        let full = root.join(&path);
        let binary;
        let mut additions = 0u32;
        let size = tokio::fs::metadata(&full)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if size > MAX_PATCH_BYTES as u64 {
            binary = true;
            truncated = true;
        } else {
            match tokio::fs::read(&full).await {
                Ok(bytes) => {
                    binary = bytes.contains(&0);
                    if !binary {
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        additions = if text.is_empty() {
                            0
                        } else {
                            (text.split('\n').count() - usize::from(text.ends_with('\n'))) as u32
                        };
                        let addition = untracked_patch(&path, &text);
                        if patch.len() + addition.len() <= MAX_PATCH_BYTES {
                            if !patch.is_empty() && !patch.ends_with('\n') {
                                patch.push('\n');
                            }
                            patch.push_str(&addition);
                        } else {
                            truncated = true;
                        }
                    }
                }
                Err(_) => continue,
            }
        }
        files.push(DiffFileSummary {
            path,
            old_path: None,
            status: "added".to_string(),
            additions,
            deletions: 0,
            binary,
        });
    }

    let additions: u32 = files.iter().map(|f| f.additions).sum();
    let deletions: u32 = files.iter().map(|f| f.deletions).sum();
    let files_json = serde_json::to_string(&files)
        .map_err(|e| EngineError::Other(format!("diff files serialize: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(branch.as_bytes());
    hasher.update([0u8]);
    hasher.update(head.as_bytes());
    hasher.update([0u8]);
    hasher.update(patch.as_bytes());
    hasher.update([0u8]);
    hasher.update(files_json.as_bytes());
    hasher.update(if truncated { b"1" } else { b"0" });
    let checksum = crate::repos::hex(&hasher.finalize());

    Ok(DiffSnapshot {
        branch,
        head_sha: (!head.is_empty()).then_some(head),
        patch,
        files,
        additions,
        deletions,
        truncated,
        checksum,
    })
}

#[cfg(test)]
mod watch_budget_tests {

    use super::{
        MAX_WATCH_DIRS, build_ignore_matcher, event_needs_capture, exceeds_watch_budget,
        parse_status_v2, split_numstat_patch,
    };
    use crate::repos::CheckoutIdentity;

    #[test]
    fn small_tree_is_watchable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/a/b")).unwrap();
        std::fs::create_dir_all(root.join("src/c")).unwrap();
        std::fs::write(root.join("src/a/f.txt"), "x").unwrap();
        assert!(!exceeds_watch_budget(root));
    }

    #[test]
    fn budget_is_exceeded_and_probe_stays_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // One flat directory of MAX_WATCH_DIRS + 50 subdirs trips the budget;
        // the BFS must stop right after the threshold, not enumerate the rest.
        for i in 0..(MAX_WATCH_DIRS + 50) {
            std::fs::create_dir(root.join(format!("d{i}"))).unwrap();
        }
        assert!(exceeds_watch_budget(root));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dir_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("real/inner")).unwrap();
        // A self-referential symlink cycle must not send the walk into a spin.
        std::os::unix::fs::symlink(root.join("real"), root.join("real/inner/loop")).unwrap();
        assert!(!exceeds_watch_budget(root)); // terminates, under budget
    }

    #[test]
    fn ignored_output_events_do_not_kick_capture() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "dist/\ntarget/\n").unwrap();
        let identity = CheckoutIdentity {
            id: "checkout".into(),
            root: tmp.path().into(),
            git_dir: tmp.path().join(".git"),
        };
        let ignored = build_ignore_matcher(tmp.path());
        let generated = notify::Event::new(notify::EventKind::Any)
            .add_path(tmp.path().join("dist/artifact.js"));
        let source =
            notify::Event::new(notify::EventKind::Any).add_path(tmp.path().join("src/lib.rs"));
        let outside = notify::Event::new(notify::EventKind::Any)
            .add_path(tmp.path().parent().unwrap().join("outside"));
        assert!(!event_needs_capture(&identity, &ignored, &generated));
        assert!(event_needs_capture(&identity, &ignored, &source));
        assert!(event_needs_capture(&identity, &ignored, &outside));

        let tracked_root = tempfile::tempdir().unwrap();
        let tracked_identity = CheckoutIdentity {
            id: "tracked-checkout".into(),
            root: tracked_root.path().into(),
            git_dir: tracked_root.path().join(".git"),
        };
        let tracked_matcher = build_ignore_matcher(tracked_root.path());
        let tracked_dist = notify::Event::new(notify::EventKind::Any)
            .add_path(tracked_root.path().join("dist/tracked.js"));
        assert!(event_needs_capture(
            &tracked_identity,
            &tracked_matcher,
            &tracked_dist,
        ));
    }

    #[test]
    fn porcelain_v2_and_combined_diff_split_without_extra_git_calls() {
        let status = b"# branch.oid abc123\0# branch.head main\0\
1 .M N... 100644 100644 100644 a b src/lib.rs\0? new file.txt\0";
        let (branch, head, files, untracked) = parse_status_v2(status);
        assert_eq!(branch, "main");
        assert_eq!(head, "abc123");
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(untracked, ["new file.txt"]);

        let combined = b"1\t0\tsrc/lib.rs\0\0diff --git a/src/lib.rs b/src/lib.rs\n";
        let (numstat, patch) = split_numstat_patch(combined);
        assert_eq!(numstat, b"1\t0\tsrc/lib.rs");
        assert!(patch.starts_with(b"diff --git "));
    }
}
