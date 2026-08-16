//! Deterministic local chat titles derived from the first user prompt.
//!
//! Titling never consults a model catalog or starts an auxiliary harness run.
//! The chat row is named immediately; an eligible Comet worktree branch is
//! renamed asynchronously without delaying the agent run.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::EngineError;
use crate::repos::Repos;
use crate::workspace_host::WorkspaceHost;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Inner {
    workspace: WorkspaceHost,
    repos: Repos,
    in_flight: Mutex<HashSet<String>>,
    /// Locally generated titles remain provisional until a harness supplies
    /// its canonical session name. The map has the same chat cardinality as
    /// the engine's standing session caches.
    generated_titles: Mutex<HashMap<String, String>>,
}

#[derive(Clone)]
pub struct TitleGenerator {
    inner: Arc<Inner>,
}

impl TitleGenerator {
    pub fn new(workspace: WorkspaceHost, repos: Repos) -> Self {
        Self {
            inner: Arc::new(Inner {
                workspace,
                repos,
                in_flight: Mutex::new(HashSet::new()),
                generated_titles: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Fire-and-forget: derive a stable local title from `prompt` if `chat_id`
    /// is still untitled. Duplicate signals for the same chat share one task.
    pub fn maybe_generate(&self, chat_id: &str, prompt: &str) {
        if !lock(&self.inner.in_flight).insert(chat_id.to_string()) {
            return;
        }
        let this = self.clone();
        let chat_id = chat_id.to_string();
        let prompt = prompt.to_string();
        tokio::spawn(async move {
            if let Err(err) = this.generate(&chat_id, &prompt).await {
                tracing::debug!(chat = %chat_id, error = %err, "chat auto-titling skipped");
            }
            lock(&this.inner.in_flight).remove(&chat_id);
        });
    }

    async fn generate(&self, chat_id: &str, prompt: &str) -> Result<(), EngineError> {
        let chat = self
            .inner
            .workspace
            .doc()
            .chat(chat_id)?
            .ok_or_else(|| EngineError::Other("chat has no workspace row".into()))?;
        if chat
            .title
            .as_deref()
            .is_some_and(|title| !title.trim().is_empty())
        {
            return Ok(());
        }

        let title = title_from_prompt(prompt);
        if title.is_empty() {
            return Ok(());
        }
        {
            let mut generated = lock(&self.inner.generated_titles);
            generated.insert(chat_id.to_string(), title.clone());
            if let Err(err) = self.inner.workspace.rename_chat(chat_id, &title) {
                generated.remove(chat_id);
                return Err(err);
            }
        }
        tracing::info!(chat = %chat_id, title = %title, "chat auto-titled locally");

        if let (Some(chat_cwd), Some(branch)) = (&chat.cwd, &chat.branch)
            && branch.starts_with("comet/")
        {
            match self
                .inner
                .repos
                .rename_worktree_branch(std::path::Path::new(chat_cwd), branch, &title)
                .await
            {
                Ok(renamed) if &renamed != branch => {
                    if let Err(err) = self.inner.workspace.set_chat_branch(chat_id, &renamed) {
                        tracing::warn!(chat = %chat_id, error = %err, "chat branch update failed");
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "automatic worktree branch rename failed");
                }
            }
        }
        Ok(())
    }

    /// Replace Comet's provisional first-prompt title with the canonical name
    /// emitted by an ACP harness. A later manual rename always wins.
    pub fn adopt_harness_title(&self, chat_id: &str, title: &str) -> Result<(), EngineError> {
        let title = title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(120)
            .collect::<String>();
        if title.is_empty() {
            return Ok(());
        }

        let mut generated = lock(&self.inner.generated_titles);
        let chat = self
            .inner
            .workspace
            .doc()
            .chat(chat_id)?
            .ok_or_else(|| EngineError::Other("chat has no workspace row".into()))?;
        let current = chat
            .title
            .as_deref()
            .filter(|current| !current.trim().is_empty());
        if current.is_some() && generated.get(chat_id).map(String::as_str) != current {
            return Ok(());
        }
        if current != Some(title.as_str()) {
            self.inner.workspace.rename_chat(chat_id, &title)?;
            tracing::info!(chat = %chat_id, title = %title, "chat adopted harness session title");
        }
        generated.remove(chat_id);
        Ok(())
    }
}

pub fn title_from_prompt(prompt: &str) -> String {
    let mut title = String::with_capacity(48);
    let mut char_count = 0;
    for word in prompt.split_whitespace().take(7) {
        if char_count == 48 {
            break;
        }
        if !title.is_empty() {
            title.push(' ');
            char_count += 1;
        }
        for character in word.chars() {
            if char_count == 48 {
                return title;
            }
            title.push(character);
            char_count += 1;
        }
    }
    title
}

#[cfg(test)]
mod tests {
    use super::title_from_prompt;

    #[test]
    fn titles_are_local_bounded_and_deterministic() {
        let prompt = "  Investigate   the staging restart and make it reliable for everyone  ";
        assert_eq!(
            title_from_prompt(prompt),
            "Investigate the staging restart and make it"
        );
        assert_eq!(title_from_prompt(prompt), title_from_prompt(prompt));
        assert!(title_from_prompt(&"x".repeat(80)).chars().count() <= 48);
        assert_eq!(title_from_prompt(" \n\t "), "");
    }
}
