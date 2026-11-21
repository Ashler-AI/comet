//! Crew: a Scaffold sandbox session's chat row (docs/crew/zgx-thin-port.md,
//! "Scaffold sandboxes"). Its controller, the desktop that made the sandbox,
//! publishes the row with Crew's `crew` field: the owner, scope and sandbox
//! environment the Edge admits the sandbox's host by
//! (`edge/src/crew/registry-scope.ts`, `bindingMatches`). Upstream's `Chat`
//! has no such field, so it is written beside the typed row, on the same
//! row.

use super::*;

impl RegistryDoc {
    /// A live chat row's Crew metadata as written (none on an upstream chat).
    pub fn crew_chat_metadata(&self, chat_id: &str) -> Option<Value> {
        self.overlay_row(KIND_CHATS, chat_id)
            .filter(|row| !row.deleted)?
            .fields
            .get("crew")
            .filter(|value| !value.is_null())
            .cloned()
    }

    /// Publish a sandbox session's chat: a new row whole (`whole`), else the
    /// existing row's host, room, folder and (when given) config; and its
    /// Crew metadata, in the same batch.
    pub fn crew_publish_chat(&mut self, chat: &Chat, crew: &Value, whole: bool) -> Result<(), DocError> {
        if whole {
            self.upsert_chat(chat)?;
            self.write(KIND_CHATS, &chat.id, OpKind::Update, fields([("crew", crew.clone())]));
            return Ok(());
        }
        let mut set = fields([
            ("deviceId", json!(chat.device_id)),
            ("roomGen", chat.room_gen.map(|g| json!(g)).unwrap_or(Value::Null)),
            ("crew", crew.clone()),
        ]);
        if let Some(cwd) = &chat.cwd {
            set.insert("cwd".into(), json!(cwd));
        }
        if let Some(config) = &chat.config {
            set.insert("config".into(), serde_json::to_value(config)?);
        }
        self.write(KIND_CHATS, &chat.id, OpKind::Update, set);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(device: &str) -> Chat {
        Chat {
            id: "chat-1".into(),
            device_id: device.into(),
            title: None,
            archived: false,
            cwd: Some("/workspace/ashler-platform".into()),
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            harness_session_harness: None,
            space_id: None,
            last_seen_at: None,
            room_gen: Some(2),
            parent_chat_id: None,
            child_kind: None,
            sleep: None,
        }
    }

    /// The row keeps the typed chat and Crew's field beside it; a later
    /// publish moves the host and replaces the metadata, and an upstream
    /// write to the row leaves the metadata as it was.
    #[test]
    fn a_sandbox_chat_carries_crew_s_metadata_beside_the_typed_row() {
        let mut doc = RegistryDoc::new("desktop");
        let crew = json!({"ownerSubject": "owner@example.com", "sessionId": "chat-1"});
        doc.crew_publish_chat(&chat("comet-scaffold-sbx-e1"), &crew, true).unwrap();
        assert_eq!(doc.chat("chat-1").unwrap().unwrap().device_id, "comet-scaffold-sbx-e1");
        assert_eq!(doc.crew_chat_metadata("chat-1"), Some(crew));
        let moved = json!({"ownerSubject": "owner@example.com", "sessionId": "chat-1", "epoch": 2});
        doc.crew_publish_chat(&chat("comet-scaffold-sbx-e2"), &moved, false).unwrap();
        doc.rename_chat("chat-1", "Renamed").unwrap();
        let row = doc.chat("chat-1").unwrap().unwrap();
        assert_eq!((row.device_id.as_str(), row.title.as_deref()), ("comet-scaffold-sbx-e2", Some("Renamed")));
        assert_eq!(doc.crew_chat_metadata("chat-1"), Some(moved));
        assert_eq!(doc.crew_chat_metadata("missing"), None);
    }
}
