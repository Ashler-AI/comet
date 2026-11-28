//! Crew: a command queued under its caller's own id, and read back by it
//! (docs/crew/zgx-thin-port.md, "Scaffold's own sessions"). Ported from
//! Crew's `queue_command_named` and `ReadCommand` (Ashler-AI/comet
//! `crates/engine/src/doc_host.rs`, `rpc.rs`).
//!
//! Scaffold's Board runs each task as a turn in its session's chat
//! (Platform's `sandbox-images/ashler-platform/runtime/crew-agent.mjs`). The
//! runner owns the command's idempotency. After a reconnect it queues the same
//! id again and must get the first command back, never a second turn. It then
//! follows the command's status by that id. A targeted interrupt names the
//! turn it stops, and is refused once that turn has passed.

use serde_json::{Value, json};
use zeron_doc::{CommandBasedOn, SessionCommandEntry, SessionCommandPayload, SessionCommandStatus};

use super::DocHost;
use crate::{EngineError, new_id, now_ms};

/// Reads one command of a chat by its id: `{"command": {id, status,
/// resolution}}`, or `{"command": null}` when the chat has none by that id.
pub const READ_COMMAND: &str = "ReadCommand";

/// The longest command or turn id a caller may name.
const MAX_ID: usize = 256;

impl DocHost {
    /// [`Self::queue_command_with_transfers`] under the caller's own id, and
    /// for an interrupt, the turn it stops. With neither, it is upstream's.
    pub(crate) fn queue_command_named(
        &self,
        chat_id: &str,
        payload: SessionCommandPayload,
        transfers: Vec<crate::uploads::AttachmentTransfer>,
        command_id: Option<&str>,
        expected_turn_id: Option<&str>,
    ) -> Result<String, EngineError> {
        if command_id.is_none() && expected_turn_id.is_none() {
            return self.queue_command_with_transfers(chat_id, payload, transfers);
        }
        if expected_turn_id.is_some_and(|turn| turn.is_empty() || turn.len() > MAX_ID)
            || (expected_turn_id.is_some() && !matches!(payload, SessionCommandPayload::Interrupt {}))
        {
            return Err(EngineError::Other("Expected turn ID requires a valid targeted interrupt".into()));
        }
        if command_id.is_some_and(|id| id.is_empty() || id.len() > MAX_ID) {
            return Err(EngineError::Other("Invalid immutable command ID".into()));
        }
        // Attachment bytes chase a command upstream's way only (its Cloud
        // hold); a caller naming its command sends none.
        if !transfers.is_empty() {
            return Err(EngineError::Other("A command queued under its own ID carries no attachments".into()));
        }
        let handle = self.open(chat_id)?;
        if let Some(id) = command_id
            && let Some(existing) = handle.doc.read_commands()?.into_iter().find(|entry| entry.id == id)
        {
            let existing_turn = existing.based_on.as_ref().and_then(|based| based.turn_id.as_deref());
            if existing.payload != payload
                || existing.issued_by != self.inner.config.device_id
                || expected_turn_id.is_some_and(|turn| existing_turn != Some(turn))
            {
                return Err(EngineError::Other("Command ID already has a different immutable request".into()));
            }
            self.nudge_remote_host(chat_id);
            return Ok(id.to_owned());
        }
        let based_on = handle.doc.read_entries_shared()?.last().map(|m| CommandBasedOn {
            turn_id: Some(m.id.clone()),
            frontier: None,
        });
        let current_turn = based_on.as_ref().and_then(|based| based.turn_id.as_deref());
        if expected_turn_id.is_some_and(|turn| current_turn != Some(turn)) {
            return Err(EngineError::Other("Targeted interrupt belongs to a past turn".into()));
        }
        let id = command_id.map_or_else(new_id, str::to_owned);
        let now = now_ms();
        let is_message = matches!(
            payload,
            SessionCommandPayload::Run { .. } | SessionCommandPayload::Steer { .. }
        );
        let entry = SessionCommandEntry {
            id: id.clone(),
            payload,
            issued_by: self.inner.config.device_id.clone(),
            issued_at: now,
            based_on,
            expires_at: Some(now + self.command_ttl(chat_id)),
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        handle.doc.queue_command(&entry)?;
        if is_message {
            self.unarchive_on_send(chat_id);
        }
        self.nudge_remote_host(chat_id);
        self.spawn_command_delivery(chat_id, entry, Vec::new());
        Ok(id)
    }

    /// The command `command_id` of `chat_id`: its id, status and resolution,
    /// or `None` when the chat has no command by that id.
    pub(crate) fn read_command(&self, chat_id: &str, command_id: &str) -> Result<Option<Value>, EngineError> {
        let handle = self.open(chat_id)?;
        Ok(handle
            .doc
            .read_commands()?
            .into_iter()
            .find(|entry| entry.id == command_id)
            .map(|entry| json!({"id": entry.id, "status": entry.status, "resolution": entry.resolution})))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use zeron_doc::{SessionCommandPayload, SessionMessageEntry};

    use super::super::{DocHost, DocHostConfig};

    fn host() -> (tempfile::TempDir, DocHost) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(zeron_sync::DocsStore::open(dir.path()).unwrap());
        let host = DocHost::new(
            store,
            DocHostConfig { device_id: "host".into(), default_harness: zeron_proto::HarnessId::Mock, edge: None },
        );
        (dir, host)
    }

    /// The runner's start: `{"kind": "run", "messageId", "request"}`, as
    /// crew-agent.mjs sends it.
    fn run(message: &str, prompt: &str) -> SessionCommandPayload {
        serde_json::from_value(json!({"kind": "run", "messageId": message, "request": {
            "prompt": prompt, "cwd": "/workspace/ashler-platform", "model": "anthropic/claude-opus-5-5",
            "reasoning": null, "modelOptions": {}, "sandbox": "danger-full-access", "autoApprove": false,
            "resume": null, "attachments": [], "harness": "omp",
        }}))
        .unwrap()
    }

    fn commands(host: &DocHost, chat: &str) -> Vec<String> {
        host.open(chat).unwrap().doc.read_commands().unwrap().into_iter().map(|entry| entry.id).collect()
    }

    /// The runner queues its start again after a reconnect: the same id gets
    /// the first command back, and the chat still has one; that command reads
    /// back by its id, pending until its host applies it.
    #[tokio::test]
    async fn a_command_queued_again_under_its_id_is_the_first_and_reads_back_by_it() {
        let (_dir, host) = host();
        let start = run("scaffold:run-1", "Fix the alert");
        let first = host.queue_command_named("session", start.clone(), Vec::new(), Some("scaffold:run-1"), None).unwrap();
        let again = host.queue_command_named("session", start, Vec::new(), Some("scaffold:run-1"), None).unwrap();
        assert_eq!((first.as_str(), again.as_str()), ("scaffold:run-1", "scaffold:run-1"));
        assert_eq!(commands(&host, "session"), ["scaffold:run-1"]);

        let read = host.read_command("session", "scaffold:run-1").unwrap().unwrap();
        assert_eq!((read["id"].as_str(), read["status"].as_str()), (Some("scaffold:run-1"), Some("pending")));
        assert_eq!(host.read_command("session", "scaffold:run-2").unwrap(), None);
    }

    /// One id, one request: another prompt under it is refused, and the chat
    /// keeps the first.
    #[tokio::test]
    async fn another_request_under_a_queued_id_is_refused() {
        let (_dir, host) = host();
        host.queue_command_named("session", run("scaffold:run-1", "First"), Vec::new(), Some("scaffold:run-1"), None)
            .unwrap();
        let refused = host
            .queue_command_named("session", run("scaffold:run-1", "Second"), Vec::new(), Some("scaffold:run-1"), None)
            .unwrap_err();
        assert!(refused.to_string().contains("different immutable request"), "{refused}");
        assert_eq!(commands(&host, "session"), ["scaffold:run-1"]);
    }

    /// A targeted interrupt stops the turn it names, and only while that turn
    /// is the chat's last; a turn id on anything else is refused.
    #[tokio::test]
    async fn a_targeted_interrupt_names_the_current_turn_or_is_refused() {
        let (_dir, host) = host();
        host.open("session").unwrap().doc.push_message(&SessionMessageEntry {
            id: "turn-1".into(),
            role: zeron_doc::MessageRole::User,
            parts: vec![zeron_doc::MessagePart::Text { id: "text".into(), text: "Fix the alert".into() }],
            created_at: 1,
            device_id: "host".into(),
            status: None,
            continuation_of: None,
            duration_ms: None,
        })
        .unwrap();
        let past = host
            .queue_command_named("session", SessionCommandPayload::Interrupt {}, Vec::new(), Some("stop-1"), Some("turn-0"))
            .unwrap_err();
        assert!(past.to_string().contains("past turn"), "{past}");
        let wrong = host
            .queue_command_named("session", run("m", "Hi"), Vec::new(), Some("run-1"), Some("turn-1"))
            .unwrap_err();
        assert!(wrong.to_string().contains("targeted interrupt"), "{wrong}");

        let stop = host
            .queue_command_named("session", SessionCommandPayload::Interrupt {}, Vec::new(), Some("stop-1"), Some("turn-1"))
            .unwrap();
        assert_eq!(stop, "stop-1");
        assert_eq!(commands(&host, "session"), ["stop-1"]);
    }

    /// Without an id or a turn, a command is queued upstream's way, under a
    /// new id.
    #[tokio::test]
    async fn an_unnamed_command_is_upstream_s() {
        let (_dir, host) = host();
        let id = host.queue_command_named("session", run("m-1", "Hi"), Vec::new(), None, None).unwrap();
        assert!(!id.is_empty() && id != "m-1");
        assert_eq!(commands(&host, "session"), [id]);
    }
}
