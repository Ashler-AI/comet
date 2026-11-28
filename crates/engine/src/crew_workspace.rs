//! Crew: a Scaffold sandbox session's chat and host rows, as its controller
//! publishes them (crew_cloud.rs; zeron-doc's `registry/crew.rs`).

use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use zeron_proto::{Chat, Device};

use super::WorkspaceHost;
use crate::EngineError;

impl WorkspaceHost {
    /// A chat row's Crew metadata (none on an upstream chat).
    pub fn crew_chat_metadata(&self, chat_id: &str) -> Option<Value> {
        self.read(|doc| doc.crew_chat_metadata(chat_id))
    }

    /// Publish a sandbox session's chat (whole when new) with its Crew
    /// metadata, and its host's device row when there's none: one batch.
    pub fn crew_publish_chat(&self, chat: &Chat, crew: &Value, whole: bool, device: Option<&Device>) -> Result<(), EngineError> {
        Ok(self.mutate(|doc| {
            doc.crew_publish_chat(chat, crew, whole)?;
            if let Some(device) = device {
                doc.upsert_device(device)?;
            }
            Ok::<_, zeron_doc::DocError>(())
        })?)
    }

    /// Each chat of `rows` (a list of chats) with its Crew metadata, when it
    /// has some, as Crew's rows carry it: one read of the registry.
    fn with_crew(&self, rows: &mut Value) {
        let Some(rows) = rows.as_array_mut() else { return };
        self.read(|doc| {
            for row in rows.iter_mut() {
                let Some(id) = row.get("id").and_then(Value::as_str).map(str::to_owned) else { continue };
                if let Some(crew) = doc.crew_chat_metadata(&id)
                    && let Some(row) = row.as_object_mut()
                {
                    row.insert("crew".into(), crew);
                }
            }
        });
    }
}

/// `WatchChats` on a sandbox host: the chat list as upstream's watch sends it
/// (the current list, then the list on every change), each chat with its Crew
/// metadata. Scaffold's runner finds its session's chat by it, and checks its
/// owner, scope and sandbox on it (Platform's `crew-agent.mjs`, `nativeContext`).
pub(crate) fn chats_with_crew(workspace: WorkspaceHost) -> BoxStream<'static, Value> {
    let rx = workspace.watch_chats();
    futures::stream::unfold((workspace, rx, false), |(workspace, mut rx, emitted)| async move {
        if emitted {
            rx.changed().await.ok()?;
        }
        let mut rows = serde_json::to_value(&*rx.borrow_and_update()).ok()?;
        workspace.with_crew(&mut rows);
        Some((rows, (workspace, rx, true)))
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt;
    use serde_json::json;
    use zeron_sync::DocsStore;

    use super::super::{WorkspaceHost, WorkspaceHostConfig};
    use super::chats_with_crew;

    fn workspace(dir: &std::path::Path) -> WorkspaceHost {
        let store = Arc::new(DocsStore::open(dir).unwrap());
        WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: "comet-scaffold-rcs_box-e1".into(),
                device_name: "Crew sandbox".into(),
                platform: "linux".into(),
                org_id: "project-a".into(),
                user_id: "alice@example.com".into(),
                edge: None,
                local_only: false,
            },
        )
        .unwrap()
    }

    fn session_chat(id: &str) -> zeron_proto::Chat {
        serde_json::from_value(json!({
            "id": id, "deviceId": "comet-scaffold-rcs_box-e1", "archived": false,
            "cwd": "/workspace/ashler-platform", "roomGen": 2, "createdAt": "2026-11-28T00:00:00Z",
            "config": {"harness": "omp", "model": "anthropic/claude-opus-5-5", "reasoning": null,
                "modelOptions": {}, "sandbox": "danger-full-access"},
        }))
        .unwrap()
    }

    /// The session's chat, as Scaffold publishes it for a sandbox it made
    /// (Platform's `crew-bootstrap.ts`), reads with its `crew` block: the
    /// fields `nativeContext` checks. A chat without Crew metadata reads as
    /// upstream's. A change sends the list again, still with it.
    #[tokio::test]
    async fn a_sandbox_host_s_chats_carry_their_crew_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = workspace(dir.path());
        let scope = json!({"projectId": "project-a", "deploymentId": "deployment-a", "sessionId": "session-1"});
        let crew = json!({
            "ownerSubject": "alice@example.com", "scope": scope, "sessionId": "session-1",
            "environment": {
                "source": {"kind": "scaffold", "sandbox_id": "rcs_box", "lifecycle": "starting", "lifecycle_epoch": 1},
                "ownerPrincipal": "alice@example.com", "scope": scope,
            },
        });
        workspace.crew_publish_chat(&session_chat("session-1"), &crew, true, None).unwrap();
        workspace.import_chat_row(&session_chat("upstream-chat")).unwrap();

        let mut chats = chats_with_crew(workspace.clone());
        let row = |rows: &serde_json::Value, id: &str| rows.as_array().and_then(|rows| rows.iter().find(|row| row["id"] == id).cloned());
        // The list as the workspace publishes it, once it shows `done`.
        let mut until = async |done: &dyn Fn(&serde_json::Value) -> bool| {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let rows = tokio::time::timeout_at(deadline, chats.next()).await.expect("the list in time").unwrap();
                if done(&rows) {
                    return rows;
                }
            }
        };

        let rows = until(&|rows| row(rows, "session-1").is_some() && row(rows, "upstream-chat").is_some()).await;
        let session = row(&rows, "session-1").unwrap();
        assert_eq!(session["crew"], crew);
        assert_eq!((session["roomGen"].clone(), session["config"]["harness"].clone()), (json!(2), json!("omp")));
        assert!(row(&rows, "upstream-chat").unwrap().get("crew").is_none());

        workspace.set_chat_archived("upstream-chat", true).unwrap();
        let rows = until(&|rows| row(rows, "upstream-chat").is_some_and(|chat| chat["archived"] == json!(true))).await;
        assert_eq!(row(&rows, "session-1").unwrap()["crew"], crew);
    }
}
