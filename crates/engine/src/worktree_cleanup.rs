//! Owner-approved delayed cleanup for Comet-managed git worktrees.
//!
//! Settling and filesystem ownership live on different devices in a shared
//! session. The archive RPC therefore writes a durable owner-attributed stage
//! into the shared workspace document. The device named by that stage follows
//! both the chat and stage watches, waits until the deadline, proves the path is
//! a Comet-managed linked checkout, rejects any checkout still referenced by an
//! active local chat, and only then removes it.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;

use comet_proto::{Chat, WorktreeDeletionStage};
use tokio::sync::watch;

use crate::now_ms;
use crate::repos::{CheckoutIdentity, Repos};
use crate::workspace_host::WorkspaceHost;

const REPAIR_INTERVAL: Duration = Duration::from_secs(120);

pub type QuiescentCheck = Arc<dyn Fn() -> bool + Send + Sync>;

struct WorktreeCleanupInner {
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    quiescent: QuiescentCheck,
}

#[derive(Clone)]
pub struct WorktreeCleanup {
    inner: Arc<WorktreeCleanupInner>,
}

struct CleanupGroup {
    identity: CheckoutIdentity,
    delete_after_ms: i64,
    chat_ids: Vec<String>,
}

impl WorktreeCleanup {
    /// Start the all-origin workspace follower plus a slow repair tick. Local
    /// commits and remote CRDT imports both converge through these watches.
    pub fn start(
        repos: Repos,
        workspace: WorkspaceHost,
        device_id: &str,
        quiescent: QuiescentCheck,
    ) -> Self {
        let cleanup = Self {
            inner: Arc::new(WorktreeCleanupInner {
                repos,
                workspace: workspace.clone(),
                device_id: device_id.to_string(),
                quiescent,
            }),
        };
        tokio::spawn(cleanup_task(
            Arc::downgrade(&cleanup.inner),
            workspace.watch_chats(),
            workspace.watch_worktree_deletions(),
        ));
        cleanup
    }

    /// Reconcile immediately (tests and explicit maintenance calls).
    pub async fn reconcile_now(&self) {
        let chats = match self.inner.workspace.doc().read_chats() {
            Ok(chats) => chats,
            Err(error) => {
                tracing::warn!(%error, "worktree cleanup: chat read failed");
                return;
            }
        };
        let stages = match self.inner.workspace.read_worktree_deletions() {
            Ok(stages) => stages,
            Err(error) => {
                tracing::warn!(%error, "worktree cleanup: stage read failed");
                return;
            }
        };
        self.reconcile_at(chats, stages, now_ms()).await;
    }

    async fn reconcile_at(
        &self,
        chats: Vec<Chat>,
        stages: Vec<WorktreeDeletionStage>,
        now_ms: i64,
    ) {
        let active = self.active_checkout_ids(&chats).await;
        let mut groups: HashMap<String, CleanupGroup> = HashMap::new();

        for stage in stages {
            if stage.owner_device_id != self.inner.device_id {
                continue;
            }
            if !Path::new(&stage.path).exists() {
                self.clear_stage(&stage.chat_id);
                continue;
            }
            let identity = match self
                .inner
                .repos
                .checkout_identity(Path::new(&stage.path))
                .await
            {
                Ok(identity) => identity,
                Err(error) => {
                    tracing::warn!(
                        chat = %stage.chat_id,
                        path = %stage.path,
                        %error,
                        "worktree cleanup: staged checkout identity failed"
                    );
                    continue;
                }
            };
            if self
                .inner
                .repos
                .managed_worktree_path(&identity.root)
                .is_none()
            {
                tracing::warn!(
                    chat = %stage.chat_id,
                    path = %stage.path,
                    "worktree cleanup: discarding unmanaged stage"
                );
                self.clear_stage(&stage.chat_id);
                continue;
            }
            let deadline = stage.delete_after.timestamp_millis();
            let group = groups
                .entry(identity.id.clone())
                .or_insert_with(|| CleanupGroup {
                    identity,
                    delete_after_ms: deadline,
                    chat_ids: Vec::new(),
                });
            group.delete_after_ms = group.delete_after_ms.max(deadline);
            group.chat_ids.push(stage.chat_id);
        }

        if !(self.inner.quiescent)() {
            return;
        }
        for (checkout_id, group) in groups {
            if group.delete_after_ms > now_ms || active.contains(&checkout_id) {
                continue;
            }
            // Re-read immediately before the destructive step. A chat-watch
            // frame can race this repair pass; the latest document state wins.
            let latest = self.inner.workspace.doc().read_chats().unwrap_or_default();
            if self
                .active_checkout_ids(&latest)
                .await
                .contains(&checkout_id)
            {
                continue;
            }
            match self
                .inner
                .repos
                .delete_managed_worktree(&group.identity.root)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        checkout = %checkout_id,
                        path = %group.identity.root.display(),
                        "deleted staged Comet worktree"
                    );
                    for chat_id in group.chat_ids {
                        self.clear_stage(&chat_id);
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        checkout = %checkout_id,
                        path = %group.identity.root.display(),
                        %error,
                        "worktree cleanup failed; keeping stage"
                    );
                }
            }
        }
    }

    async fn active_checkout_ids(&self, chats: &[Chat]) -> HashSet<String> {
        let mut by_cwd: HashSet<&str> = HashSet::new();
        for chat in chats {
            if chat.device_id == self.inner.device_id
                && !chat.archived
                && let Some(cwd) = chat.cwd.as_deref()
            {
                by_cwd.insert(cwd);
            }
        }
        let mut ids = HashSet::new();
        for cwd in by_cwd {
            if let Ok(identity) = self.inner.repos.checkout_identity(Path::new(cwd)).await {
                ids.insert(identity.id);
            }
        }
        ids
    }

    fn clear_stage(&self, chat_id: &str) {
        if let Err(error) = self.inner.workspace.remove_owned_worktree_deletion(chat_id) {
            tracing::warn!(chat = %chat_id, %error, "worktree cleanup: stage removal failed");
        }
    }
}

async fn cleanup_task(
    inner: Weak<WorktreeCleanupInner>,
    mut chats_rx: watch::Receiver<Vec<Chat>>,
    mut stages_rx: watch::Receiver<Vec<WorktreeDeletionStage>>,
) {
    let mut repair = tokio::time::interval(REPAIR_INTERVAL);
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    repair.tick().await;

    loop {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let cleanup = WorktreeCleanup { inner };
        cleanup.reconcile_now().await;
        drop(cleanup);
        tokio::select! {
            changed = chats_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let _ = chats_rx.borrow_and_update();
            }
            changed = stages_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let _ = stages_rx.borrow_and_update();
            }
            _ = repair.tick() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_host::WorkspaceHostConfig;
    use comet_sync::DocsStore;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn staged_worktree_waits_for_deadline_and_active_references() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "seed\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "seed"]);

        let repos =
            Repos::with_worktrees_root(temp.path(), "device-a", temp.path().join("worktrees"));
        let worktree = repos.create_worktree(&repo, "main").await.unwrap();
        let store = Arc::new(DocsStore::open(temp.path().join("store")).unwrap());
        let workspace = WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: "device-a".into(),
                device_name: "test".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "owner".into(),
                edge: None,
            },
        )
        .unwrap();
        workspace
            .create_space("space-a", "device-a", &repo.to_string_lossy(), None, true)
            .unwrap();
        workspace
            .create_chat("chat-a", "space-a", None, Some(worktree.path.clone()))
            .unwrap();
        let deadline = chrono::DateTime::from_timestamp_millis(10_000).unwrap();
        let stage = WorktreeDeletionStage {
            chat_id: "chat-a".into(),
            path: worktree.path.clone(),
            owner_subject: "owner".into(),
            owner_device_id: "device-a".into(),
            delete_after: deadline,
        };
        workspace
            .set_chat_archived_with_worktree_deletion("chat-a", true, Some(&stage))
            .unwrap();
        let cleanup = WorktreeCleanup {
            inner: Arc::new(WorktreeCleanupInner {
                repos: repos.clone(),
                workspace: workspace.clone(),
                device_id: "device-a".into(),
                quiescent: Arc::new(|| true),
            }),
        };

        let chats = workspace.doc().read_chats().unwrap();
        cleanup
            .reconcile_at(chats.clone(), vec![stage.clone()], 9_999)
            .await;
        assert!(Path::new(&worktree.path).exists());

        workspace.set_chat_archived("chat-a", false).unwrap();
        cleanup
            .reconcile_at(
                workspace.doc().read_chats().unwrap(),
                vec![stage.clone()],
                10_000,
            )
            .await;
        assert!(Path::new(&worktree.path).exists());

        workspace
            .set_chat_archived_with_worktree_deletion("chat-a", true, Some(&stage))
            .unwrap();
        cleanup
            .reconcile_at(workspace.doc().read_chats().unwrap(), vec![stage], 10_000)
            .await;
        assert!(!Path::new(&worktree.path).exists());
        assert!(workspace.read_worktree_deletions().unwrap().is_empty());
    }
}
