//! Session doc schema over `loro` — Rust port of `packages/session-doc/src/schema.ts`.
//!
//! Container layout (MUST stay shape-compatible with the TS edge/tail materializer):
//! - `meta`:     LoroMap  { chatId: string, schemaVersion: number }         (host-only writer)
//! - `messages`: LoroList of LoroMap {
//!   id, role, parts: LoroList<part map>, createdAt, deviceId, status?, continuationOf?, peerMessage? }
//! - `commands`: LoroList of LoroMap {
//!   id, kind, payload(json), issuedBy, issuedAt, basedOn?, expiresAt?, status, resolution? }
//!
//! Part maps: { id, kind: "text"|"tool"|"input"|"error", text?: LoroText, call?: LoroMap,
//! isError?, questions?: json, resolved?, message? }. Tool-call objects/arrays use nested
//! LoroMap/LoroList and strings use LoroText; legacy scalar call JSON remains readable.
//! Deep JSON is unchanged. Growing text and tool arguments append RLE-merged suffixes.

use std::collections::HashSet;

use comet_proto::{
    COLLABORATION_SCHEMA_VERSION, CollaborationSnapshot, PeerMessageProvenance, PublicationRecord,
    PublicationValue,
};
use loro::{
    ExportMode, LoroDoc, LoroError, LoroList, LoroMap, LoroText, LoroValue, TextDelta, ToJson,
    cursor::PosType,
};
use serde::{Deserialize, Serialize};

use crate::collaboration::validate_publication;
use crate::commands::{SessionCommandEntry, SessionCommandStatus};
use crate::constants::{SESSION_SCHEMA_VERSION, TAIL_MESSAGE_COUNT, TAIL_TEXT_BYTE_BUDGET};
use crate::parts::{MessagePart, MessageStatus};
use crate::SharedDocument;

#[derive(Debug, thiserror::Error)]
pub enum DocError {
    #[error("loro: {0}")]
    Loro(#[from] LoroError),
    #[error("schema: {0}")]
    Schema(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

/// One entry in the doc's `messages` list (`SessionMessageEntry` in TS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessageEntry {
    pub id: String,
    pub role: MessageRole,
    pub parts: Vec<MessagePart>,
    /// Epoch millis.
    pub created_at: i64,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MessageStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_of: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_peer_message"
    )]
    pub peer_message: Option<PeerMessageProvenance>,
}

impl SessionMessageEntry {
    /// Only explicitly identified user peer messages are collapsed in Crew.
    /// Orphan continuations and historical entries without metadata stay visible.
    pub fn is_peer_message(&self) -> bool {
        self.role == MessageRole::User
            && self.continuation_of.is_none()
            && self.peer_message.as_ref().is_some_and(|peer| {
                !self.id.trim().is_empty()
                    && peer.command_id == self.id
                    && !peer.source_chat_id.trim().is_empty()
                    && !peer.thread_id.trim().is_empty()
                    && peer
                        .reply_to
                        .as_ref()
                        .is_none_or(|id| !id.trim().is_empty())
            })
    }
}

// Invalid or unsupported optional provenance must not discard the containing row.
fn deserialize_peer_message<'de, D>(
    deserializer: D,
) -> Result<Option<PeerMessageProvenance>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// A newest-first transcript page projected back into chronological order.
/// `before` is an opaque raw-list cursor for the next older page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntryWindow {
    pub entries: Vec<SessionMessageEntry>,
    pub before: Option<usize>,
}

/// The doc-resident flat part map (`DocMessagePart` in TS). Distinct from the app-layer
/// [`MessagePart`]: input parts key on their request id, error parts store `message`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocPartJson {
    id: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    questions: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed: Option<bool>,
}

/// App parts → doc part json (mirror of `toDocParts`).
fn to_doc_part(part: &MessagePart) -> Result<DocPartJson, DocError> {
    Ok(match part {
        MessagePart::Text { id, text } => DocPartJson {
            id: id.clone(),
            kind: "text".into(),
            text: Some(text.clone()),
            ..Default::default()
        },
        MessagePart::TextWindow { .. } => {
            return Err(DocError::Schema(
                "projected text windows cannot be persisted".into(),
            ));
        }
        MessagePart::Thinking {
            id,
            started,
            completed,
        } => DocPartJson {
            id: id.clone(),
            kind: "thinking".into(),
            started: Some(*started),
            completed: Some(*completed),
            ..Default::default()
        },
        MessagePart::Tool {
            id,
            call,
            is_error,
            resolved,
        } => DocPartJson {
            id: id.clone(),
            kind: "tool".into(),
            call: Some(serde_json::to_value(call)?),
            // TS shape parity: `isError` is written only once the tool result arrived;
            // its presence IS the resolution marker.
            is_error: if *resolved { Some(*is_error) } else { None },
            ..Default::default()
        },
        MessagePart::Input {
            id: _,
            request_id,
            questions,
            resolved,
        } => DocPartJson {
            id: request_id.clone(),
            kind: "input".into(),
            questions: Some(serde_json::to_value(questions)?),
            resolved: Some(*resolved),
            ..Default::default()
        },
        MessagePart::Error { id, message } => DocPartJson {
            id: id.clone(),
            kind: "error".into(),
            message: Some(message.clone()),
            ..Default::default()
        },
    })
}

/// Doc part json → app part (mirror of `fromDocParts`; malformed degrades to empty text).
fn from_doc_part(p: DocPartJson) -> MessagePart {
    match p.kind.as_str() {
        "tool" => match p.call.and_then(|c| serde_json::from_value(c).ok()) {
            Some(call) => MessagePart::Tool {
                id: p.id,
                call,
                is_error: p.is_error.unwrap_or(false),
                resolved: p.is_error.is_some(),
            },
            None => MessagePart::Text {
                id: p.id,
                text: String::new(),
            },
        },
        "thinking" => MessagePart::Thinking {
            id: p.id,
            started: p.started.unwrap_or(true),
            completed: p.completed.unwrap_or(false),
        },
        "input" => MessagePart::Input {
            id: p.id.clone(),
            request_id: p.id,
            questions: p
                .questions
                .and_then(|q| serde_json::from_value(q).ok())
                .unwrap_or_default(),
            resolved: p.resolved.unwrap_or(false),
        },
        "error" => MessagePart::Error {
            id: p.id,
            message: p.message.unwrap_or_default(),
        },
        _ => MessagePart::Text {
            id: p.id,
            text: p.text.unwrap_or_default(),
        },
    }
}

/// A session doc handle: typed access over a LoroDoc with the schema above.
pub struct SessionDoc {
    doc: SharedDocument,
}

impl SessionDoc {
    /// Wrap an existing doc (e.g. imported from a snapshot) and perform the additive v2
    /// cutover. Existing/unknown containers and fields are untouched.
    pub fn from_doc(doc: LoroDoc) -> Self {
        let meta = doc.get_map("meta");
        let current = match meta.get("schemaVersion") {
            Some(loro::ValueOrContainer::Value(LoroValue::I64(value))) => value,
            _ => 0,
        };
        if current < i64::from(SESSION_SCHEMA_VERSION) {
            if let Err(error) = meta.insert("schemaVersion", i64::from(SESSION_SCHEMA_VERSION)) {
                tracing::warn!(%error, "session schema version cutover failed");
            } else {
                doc.commit();
            }
        }
        Self { doc: SharedDocument::new(doc) }
    }

    /// Create + initialize a fresh doc for `chat_id` (host-only).
    pub fn init(chat_id: &str) -> Result<Self, DocError> {
        let doc = LoroDoc::new();
        let meta = doc.get_map("meta");
        meta.insert("chatId", chat_id)?;
        meta.insert("schemaVersion", SESSION_SCHEMA_VERSION as i64)?;
        meta.insert("directoryCompletedTurn", "")?;
        doc.commit();
        Ok(Self { doc: SharedDocument::new(doc) })
    }

    /// A raw snapshot handle; typed mutators use the replaceable binding.
    pub fn doc(&self) -> LoroDoc {
        self.doc.raw()
    }

    pub fn binding(&self) -> SharedDocument {
        self.doc.clone()
    }

    pub fn set_completed_turn(&self, entry_id: &str) -> Result<(), DocError> {
        let _operation = self.doc.operation();
        self.doc.get_map("meta").insert("directoryCompletedTurn", entry_id)?;
        self.doc.commit()
    }

    pub fn chat_id(&self) -> Option<String> {
        match self.doc.get_map("meta").get("chatId") {
            Some(loro::ValueOrContainer::Value(LoroValue::String(s))) => Some(s.to_string()),
            _ => None,
        }
    }

    /// Insert one complete message entry (user/system messages, command-side
    /// inserts). Streaming assistant entries go through [`SegmentWriter`].
    pub fn push_message(&self, entry: &SessionMessageEntry) -> Result<(), DocError> {
        self.push_messages(std::slice::from_ref(entry))
    }

    /// Insert complete entries in one Loro commit. Native history imports use
    /// this path so a thousand-message attach produces one publish/snapshot
    /// wake instead of a thousand intermediate transcript states.
    pub fn push_messages(&self, entries: &[SessionMessageEntry]) -> Result<(), DocError> {
        let _operation = self.doc.operation();
        let existing_ids = (entries.len() > 1).then(|| self.message_ids());
        let mut fresh = std::collections::HashMap::new();
        for entry in entries {
            if let Some(previous) = fresh.get(entry.id.as_str()) {
                if *previous != entry { return Err(DocError::Schema("message id names conflicting accepted content".into())); }
                continue;
            }
            if existing_ids.as_ref().is_none_or(|ids| ids.contains(&entry.id))
                && let Some(previous) = self.read_entry(&entry.id)?
            {
                if previous != *entry { return Err(DocError::Schema("message id names conflicting accepted content".into())); }
                continue;
            }
            fresh.insert(entry.id.as_str(),entry);
        }
        let has_fresh = !fresh.is_empty();
        let messages = self.doc.get_list("messages");
        for entry in entries {
            if fresh.remove(entry.id.as_str()).is_none() { continue; }
            let map = messages.push_container(LoroMap::new())?;
            write_entry_scalar_fields(&map, entry)?;
            let parts = map.insert_container("parts", LoroList::new())?;
            for part in &entry.parts {
                push_part(&parts, part)?;
            }
        }
        if has_fresh {
            self.doc.commit()?;
        }
        Ok(())
    }

    /// Replay an accepted message against a replacement snapshot by stable identity.
    /// Streaming prefixes may advance; terminal content and immutable identity cannot diverge.
    pub fn reconcile_message(&self, entry: &SessionMessageEntry) -> Result<(), DocError> {
        let _operation = self.doc.operation();
        let messages = self.doc.get_list("messages");
        for index in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) = messages.get(index) else { continue; };
            if optional_string(&map, "id").as_deref() != Some(entry.id.as_str()) { continue; }
            let current = entry_from_json(map.get_deep_value().to_json_value())?;
            if current == *entry { return Ok(()); }
            let conflict = || DocError::Schema(format!(
                "message {} conflicts with replacement snapshot (roles={:?}/{:?}, status={:?}/{:?}, identity_equal=[{},{},{},{}], parts={}/{}, prefixes={}/{})",
                entry.id, current.role, entry.role, current.status, entry.status,
                current.device_id == entry.device_id, current.created_at == entry.created_at,
                current.continuation_of == entry.continuation_of, current.peer_message == entry.peer_message,
                current.parts.len(), entry.parts.len(), parts_are_prefix(&current.parts, &entry.parts), parts_are_prefix(&entry.parts, &current.parts),
            ));
            if current.device_id != entry.device_id || current.role != entry.role
                || current.created_at != entry.created_at || current.continuation_of != entry.continuation_of
                || current.peer_message != entry.peer_message {
                return Err(conflict());
            }
            if current.role != MessageRole::Assistant && current.parts == entry.parts {
                match (current.status,entry.status) {
                    (Some(MessageStatus::Complete | MessageStatus::Aborted),Some(MessageStatus::Queued | MessageStatus::Steered) | None)
                    | (Some(MessageStatus::Steered),Some(MessageStatus::Queued) | None) => return Ok(()),
                    (Some(MessageStatus::Queued | MessageStatus::Steered) | None,Some(MessageStatus::Complete | MessageStatus::Aborted | MessageStatus::Steered)) => {
                        map.insert("status",status_str(entry.status.expect("matched delivery status")))?;
                        return self.doc.commit();
                    }
                    _ => return Err(conflict()),
                }
            }
            if current.role != MessageRole::Assistant { return Err(conflict()); }
            let interruption = entry.parts.split_last().filter(|(last, _)|
                entry.status == Some(MessageStatus::Aborted) && matches!(last, MessagePart::Error { .. }));
            if current.status != Some(MessageStatus::Streaming) {
                if matches!(current.status, Some(MessageStatus::Complete | MessageStatus::Aborted))
                    && ((entry.status == Some(MessageStatus::Streaming)
                        && (parts_are_prefix(&entry.parts, &current.parts) || parts_are_prefix(&current.parts, &entry.parts)))
                        || interruption.is_some_and(|(_, prefix)| parts_are_prefix(prefix, &current.parts))) {
                    return Ok(()); // The remote owner already committed a terminal outcome.
                }
                return Err(conflict());
            }
            // A checkpoint may trail acknowledged streaming content. Its
            // retained prefix is already satisfied, not a competing rewrite.
            if entry.status == Some(MessageStatus::Streaming)
                && parts_are_prefix(&entry.parts, &current.parts) {
                return Ok(());
            }
            // Restart recovery closes a retained checkpoint, not the richer
            // acknowledged stream. Keep that stream and append its interruption.
            let interruption = interruption.filter(|(_, prefix)| parts_are_prefix(prefix, &current.parts));
            if interruption.is_none() && (!matches!(entry.status, Some(MessageStatus::Streaming | MessageStatus::Complete | MessageStatus::Aborted))
                || !parts_are_prefix(&current.parts, &entry.parts)) { return Err(conflict()); }
            let parts = match map.get("parts") {
                Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) => parts,
                _ => return Err(conflict()),
            };
            if let Some((error, _)) = interruption {
                push_part(&parts, error)?;
            } else {
                if parts.len() > 0 { parts.delete(0, parts.len())?; }
                for part in &entry.parts { push_part(&parts, part)?; }
            }
            map.insert("status", status_str(entry.status.expect("checked message status")))?;
            self.doc.commit()?;
            return Ok(());
        }
        let map = messages.push_container(LoroMap::new())?;
        write_entry_scalar_fields(&map, entry)?;
        let parts = map.insert_container("parts", LoroList::new())?;
        for part in &entry.parts { push_part(&parts, part)?; }
        self.doc.commit()
    }

    /// Read message ids without materializing parts or text bodies.
    pub fn message_ids(&self) -> HashSet<String> {
        let messages = self.doc.get_list("messages");
        let mut ids = HashSet::with_capacity(messages.len());
        for index in 0..messages.len() {
            if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                messages.get(index)
                && let Some(loro::ValueOrContainer::Value(LoroValue::String(id))) = map.get("id")
            {
                ids.insert(id.to_string());
            }
        }
        ids
    }

    pub fn directory_entry_count(&self) -> usize {
        self.doc.get_list("messages").len()
    }

    /// Bounded title projection; link extraction uses directory_visit_text so
    /// oversized text is scanned fully rather than silently omitted.
    pub fn directory_entry(&self, index: usize) -> Result<Option<SessionMessageEntry>, DocError> {
        let Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) = self.doc.get_list("messages").get(index) else {
            return Err(DocError::Schema("malformed directory transcript row".into()));
        };
        if optional_string(&row, "role").as_deref() == Some("system") { return Ok(None); }
        let mut budget = 256 * 1024;
        let entry = entry_from_map_window(&row, &mut budget)?;
        if entry.parts.iter().filter(|part| !matches!(part, MessagePart::TextWindow { .. } | MessagePart::Text { .. }))
            .map(MessagePart::byte_len).sum::<usize>() > 256 * 1024
        {
            return Err(DocError::Schema("directory transcript row exceeds scan budget".into()));
        }
        Ok(Some(entry))
    }

    pub fn directory_visit_text(&self, index: usize, mut visit: impl FnMut(&str, bool) -> Result<(), DocError>) -> Result<(), DocError> {
        let Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) = self.doc.get_list("messages").get(index) else {
            return Err(DocError::Schema("malformed directory transcript row".into()));
        };
        if optional_string(&row, "role").as_deref() == Some("system") { return Ok(()); }
        let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) = row.get("parts") else { return Ok(()) };
        for index in 0..parts.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) = parts.get(index) else { continue };
            if optional_string(&part, "kind").as_deref() != Some("text") { continue; }
            let Some(loro::ValueOrContainer::Container(loro::Container::Text(text))) = part.get("text") else { continue };
            let total = text.len_utf8();
            let mut start = 0;
            while start < total {
                let mut end = (start + 64 * 1024).min(total);
                let delta = loop {
                    match text.slice_delta(start, end, PosType::Bytes) {
                        Ok(delta) => break delta,
                        Err(_) if end > start + 1 => { end -= 1; },
                        Err(_) => return Err(DocError::Schema("invalid directory text boundary".into())),
                    }
                };
                for delta in delta {
                    if let TextDelta::Insert { insert, .. } = delta { visit(&insert, false)?; }
                }
                start = end;
            }
            visit("", true)?;
        }
        Ok(())
    }

    /// Read all entries (continuations NOT joined — see `join_continuation_entries`).
    ///
    /// Malformed entries are SKIPPED, not fatal: a torn intermediate state
    /// (an entry map imported before the update that fills its fields) or a
    /// peer on a newer schema must degrade to a missing row, never blank the
    /// whole transcript — one bad entry took down every publish for the chat
    /// (2026-07-31, "missing field `id`" during a multi-update import).
    pub fn read_entries(&self) -> Result<Vec<SessionMessageEntry>, DocError> {
        // Materialize only the messages container — a whole-doc deep value
        // here also serialized the commands ledger on every 120ms commit tick.
        let messages = self
            .doc
            .get_list("messages")
            .get_deep_value()
            .to_json_value();
        let raw: Vec<serde_json::Value> = serde_json::from_value(messages)?;
        Ok(raw
            .into_iter()
            .filter_map(|v| match entry_from_json(v) {
                Ok(entry) => Some(entry),
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed transcript entry");
                    None
                }
            })
            .collect())
    }

    /// Read the final valid raw entry's id without materializing earlier history.
    /// Command ancestry refers to raw continuation rows, not their joined root.
    pub fn last_message_id(&self) -> Option<String> {
        let messages = self.doc.get_list("messages");
        for index in (0..messages.len()).rev() {
            let Some(row) = messages.get(index) else {
                continue;
            };
            match entry_from_json(row.get_deep_value().to_json_value()) {
                Ok(entry) => return Some(entry.id),
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed transcript entry");
                }
            }
        }
        None
    }

    /// Read one raw entry without materializing unrelated messages or joining continuations.
    /// Malformed matching rows are skipped, just as in `read_entries`.
    pub fn read_entry(&self, message_id: &str) -> Result<Option<SessionMessageEntry>, DocError> {
        let messages = self.doc.get_list("messages");
        for index in 0..messages.len() {
            let Some(row) = messages.get(index) else {
                continue;
            };
            let matches_id = match &row {
                loro::ValueOrContainer::Container(loro::Container::Map(map)) => matches!(
                    map.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(id)))
                        if id.as_str() == message_id
                ),
                loro::ValueOrContainer::Value(LoroValue::Map(map)) => matches!(
                    map.get("id"),
                    Some(LoroValue::String(id)) if id.as_str() == message_id
                ),
                _ => false,
            };
            if matches_id {
                match entry_from_json(row.get_deep_value().to_json_value()) {
                    Ok(entry) => return Ok(Some(entry)),
                    Err(err) => {
                        tracing::warn!(error = %err, "skipping malformed transcript entry");
                    }
                }
            }
        }
        Ok(None)
    }

    /// Check a message id without decoding unrelated message bodies. Matching
    /// rows still use the full decoder: a torn row must not suppress an append.
    pub fn contains_message(&self, message_id: &str) -> bool {
        self.read_entry(message_id).ok().flatten().is_some()
    }

    /// Full original for an explicit reveal. Only this root and its compatible
    /// continuations are decoded; normal transcript windows remain byte-bounded.
    pub fn read_message(&self, message_id: &str) -> Result<Option<SessionMessageEntry>, DocError> {
        let Some(root) = self.read_entry(message_id)? else {
            return Ok(None);
        };
        if root.continuation_of.is_some() {
            return Ok(Some(root));
        }
        let messages = self.doc.get_list("messages");
        let mut entries = vec![root];
        for index in 0..messages.len() {
            let Some(row) = messages.get(index) else {
                continue;
            };
            let is_continuation = match &row {
                loro::ValueOrContainer::Container(loro::Container::Map(map)) => matches!(
                    map.get("continuationOf"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(id))) if id.as_str() == message_id
                ),
                loro::ValueOrContainer::Value(LoroValue::Map(map)) => matches!(
                    map.get("continuationOf"),
                    Some(LoroValue::String(id)) if id.as_str() == message_id
                ),
                _ => false,
            };
            if is_continuation {
                match entry_from_json(row.get_deep_value().to_json_value()) {
                    Ok(entry) => entries.push(entry),
                    Err(err) => tracing::warn!(error = %err, "skipping malformed transcript entry"),
                }
            }
        }
        Ok(join_continuation_entries(entries).into_iter().next())
    }

    /// Read at most `max_messages` joined messages ending before the raw-list
    /// cursor `before` (`None` = current tail). Entries are materialized one at
    /// a time from the end of the Loro list, so opening a chat never converts
    /// the entire transcript container into JSON.
    ///
    /// Continuation rows do not consume the message limit. Scanning stops only
    /// after their root row is included, so a page never starts with a partial
    /// joined message when it is resumed using the returned cursor.
    pub fn read_entry_window(
        &self,
        before: Option<usize>,
        max_messages: usize,
    ) -> Result<SessionEntryWindow, DocError> {
        let messages = self.doc.get_list("messages");
        let mut cursor = before.unwrap_or_else(|| messages.len()).min(messages.len());
        let max_messages = max_messages.max(1);
        let mut roots = 0usize;
        let mut text_budget = TAIL_TEXT_BYTE_BUDGET;
        let mut reversed = Vec::with_capacity(max_messages);

        while cursor > 0 && roots < max_messages {
            cursor -= 1;
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                messages.get(cursor)
            else {
                continue;
            };
            match entry_from_map_window(&map, &mut text_budget) {
                Ok(entry) => {
                    if entry.continuation_of.is_none() {
                        roots += 1;
                    }
                    reversed.push(entry);
                }
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed transcript entry");
                }
            }
        }

        reversed.reverse();
        Ok(SessionEntryWindow {
            entries: join_continuation_entries(reversed),
            before: (cursor > 0).then_some(cursor),
        })
    }

    /// Append one immutable collaboration publication. Publication ids are durable
    /// idempotency keys: replay/reconnect of the same record is a no-op and can never
    /// remove a message or an earlier publication.
    pub fn append_publication(&self, record: &PublicationRecord) -> Result<bool, DocError> {
        let _operation = self.doc.operation();
        validate_publication(record)?;
        let publications = self.doc.get_list("publications");
        for index in 0..publications.len() {
            if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                publications.get(index)
                && matches!(
                    map.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(id))) if id.as_str() == record.id
                )
            {
                return Ok(false);
            }
        }
        let map = publications.push_container(LoroMap::new())?;
        map.insert("id", record.id.as_str())?;
        map.insert(
            "record",
            loro_value_from_json(&serde_json::to_value(record)?),
        )?;
        self.doc.commit()?;
        Ok(true)
    }

    /// Replace liveness for one anchored owner without growing immutable history.
    /// Actual owner/phase transitions remain append-only publications.
    pub fn upsert_agent_session(&self, record: &PublicationRecord) -> Result<bool, DocError> {
        let _operation = self.doc.operation();
        validate_publication(record)?;
        let PublicationValue::AgentSession(session) = &record.value else {
            return Err(DocError::Schema("owner status must be an agent session".into()));
        };
        let publications = self.read_publications()?;
        let anchors = session_anchors(&publications);
        let anchor = anchors.get(session.session_id.as_str()).copied()
            .ok_or_else(|| DocError::Schema("owner status has no immutable session anchor".into()))?;
        if record.published_by != anchor.owner_subject || !same_session_owner(anchor, session) {
            return Err(DocError::Schema("owner status does not match its immutable session anchor".into()));
        }
        let at = session.updated_at.unwrap_or(session.created_at);
        let anchored_at = anchor.updated_at.unwrap_or(anchor.created_at);
        if at < anchored_at || (at == anchored_at && session.status != anchor.status) { return Ok(false); }
        let register = self.doc.get_map("agentSessions");
        let value = serde_json::to_value(record)?;
        if let Some(previous) = register.get(&session.session_id) {
            let previous = previous.get_deep_value().to_json_value();
            if previous == value { return Ok(false); }
            if let Ok(previous) = serde_json::from_value::<PublicationRecord>(previous)
                && let PublicationValue::AgentSession(previous) = previous.value
                && same_session_owner(anchor, &previous)
                && previous.updated_at.unwrap_or(previous.created_at) > at {
                return Ok(false);
            }
        }
        register.insert(&session.session_id, loro_value_from_json(&value))?;
        self.doc.commit()?;
        Ok(true)
    }

    /// Materialize collaboration publications in append order. Malformed or
    /// over-bound future rows are skipped independently, like transcript rows.
    pub fn read_publications(&self) -> Result<Vec<PublicationRecord>, DocError> {
        #[derive(Deserialize)]
        struct RawPublication {
            record: PublicationRecord,
        }

        let value = self
            .doc
            .get_list("publications")
            .get_deep_value()
            .to_json_value();
        let rows: Vec<serde_json::Value> = serde_json::from_value(value)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let raw: RawPublication = match serde_json::from_value(row) {
                    Ok(raw) => raw,
                    Err(error) => {
                        tracing::warn!(%error, "skipping malformed collaboration publication");
                        return None;
                    }
                };
                match validate_publication(&raw.record) {
                    Ok(()) => Some(raw.record),
                    Err(error) => {
                        tracing::warn!(%error, "skipping over-bound collaboration publication");
                        None
                    }
                }
            })
            .collect())
    }

    pub fn collaboration_snapshot(&self) -> Result<CollaborationSnapshot, DocError> {
        let publications = self.read_publications()?;
        let mut sessions_by_id: std::collections::BTreeMap<_, _> = session_anchors(&publications)
            .into_iter().map(|(id, session)| (id.to_string(), session.clone())).collect();
        let mut message_provenance = Vec::new();
        for publication in &publications {
            match &publication.value {
                PublicationValue::MessageProvenance(provenance) => {
                    message_provenance.push(provenance.clone());
                }
                _ => {}
            }
        }
        if let serde_json::Value::Object(register) = self.doc.get_map("agentSessions").get_deep_value().to_json_value() {
            for (id, value) in register {
                let Ok(record) = serde_json::from_value::<PublicationRecord>(value) else { continue; };
                if validate_publication(&record).is_err() { continue; }
                let PublicationValue::AgentSession(session) = record.value else { continue; };
                if session.session_id != id { continue; }
                if let Some(anchor) = sessions_by_id.get(&id)
                    && record.published_by == anchor.owner_subject
                    && same_session_owner(anchor, &session)
                    && (session.updated_at.unwrap_or(session.created_at) > anchor.updated_at.unwrap_or(anchor.created_at)
                        || (session.updated_at.unwrap_or(session.created_at) == anchor.updated_at.unwrap_or(anchor.created_at)
                            && session.status == anchor.status)) {
                    sessions_by_id.insert(id, *session);
                }
            }
        }
        let sessions = sessions_by_id.into_values().collect();
        Ok(CollaborationSnapshot {
            schema_version: COLLABORATION_SCHEMA_VERSION,
            sessions,
            message_provenance,
            publications,
            participants: Vec::new(),
            principal: None,
            grants: Vec::new(),
        })
    }

    /// Read the commands ledger.
    ///
    /// Same skip-not-fail policy as `read_entries`: any device can append
    /// here, and one malformed entry must not wedge command draining for the
    /// chat forever (an unparseable command can't be executed anyway).
    pub fn read_commands(&self) -> Result<Vec<SessionCommandEntry>, DocError> {
        // Container-scoped for the same reason as `read_entries`: the drain
        // loop runs this per tick and must not pay for the transcript.
        let commands = self
            .doc
            .get_list("commands")
            .get_deep_value()
            .to_json_value();
        let raw: Vec<serde_json::Value> = serde_json::from_value(commands)?;
        Ok(raw
            .into_iter()
            .filter_map(|v| match serde_json::from_value(v) {
                Ok(entry) => Some(entry),
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed command entry");
                    None
                }
            })
            .collect())
    }

    /// Read one typed command without materializing unrelated ledger payloads.
    /// Malformed matching rows are skipped, just as in `read_commands`.
    pub fn read_command(&self, command_id: &str) -> Result<Option<SessionCommandEntry>, DocError> {
        let commands = self.doc.get_list("commands");
        for index in 0..commands.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                commands.get(index)
            else {
                continue;
            };
            if !matches!(
                map.get("id"),
                Some(loro::ValueOrContainer::Value(LoroValue::String(id))) if id.as_str() == command_id
            ) {
                continue;
            }
            match serde_json::from_value(map.get_deep_value().to_json_value()) {
                Ok(entry) => return Ok(Some(entry)),
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed command entry");
                }
            }
        }
        Ok(None)
    }

    /// Append a command entry (rule 1: own entries only, append-only).
    pub fn queue_command(&self, entry: &SessionCommandEntry) -> Result<(), DocError> {
        let _operation = self.doc.operation();
        if self
            .read_commands()?
            .iter()
            .any(|existing| existing.id == entry.id)
        {
            return Ok(());
        }
        if let Some(error) = self.doc.recovery_error() {
            return Err(DocError::Schema(error));
        }
        const MAX_COMMAND_METADATA_BYTES: usize = 256 * 1024;
        if entry.id.is_empty() || entry.id.len() > 256 || entry.issued_by.len() > 512 {
            return Err(DocError::Schema(
                "command identity metadata exceeds bounds".into(),
            ));
        }
        let payload_json = serde_json::to_value(&entry.payload)?;
        if serde_json::to_vec(&payload_json)?.len() > MAX_COMMAND_METADATA_BYTES {
            return Err(DocError::Schema("command payload exceeds 256 KiB".into()));
        }
        let commands = self.doc.get_list("commands");
        let map = commands.push_container(LoroMap::new())?;
        map.insert("id", entry.id.as_str())?;
        map.insert(
            "kind",
            serde_json::to_value(entry.kind())?
                .as_str()
                .ok_or_else(|| DocError::Schema("kind not a string".into()))?,
        )?;
        map.insert("payload", loro_value_from_json(&payload_json))?;
        map.insert("issuedBy", entry.issued_by.as_str())?;
        map.insert("issuedAt", entry.issued_at)?;
        if let Some(based_on) = &entry.based_on {
            map.insert(
                "basedOn",
                loro_value_from_json(&serde_json::to_value(based_on)?),
            )?;
        }
        if let Some(expires_at) = entry.expires_at {
            map.insert("expiresAt", expires_at)?;
        }
        map.insert(
            "status",
            serde_json::to_value(entry.status)?
                .as_str()
                .ok_or_else(|| DocError::Schema("status not a string".into()))?,
        )?;
        self.doc.commit()?;
        Ok(())
    }

    /// Rule 2: host (or the issuing composer, for `cancelled`) writes an outcome.
    pub fn set_command_status(
        &self,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) -> Result<(), DocError> {
        let _operation = self.doc.operation();
        if command_id.len() > 256 || resolution.is_some_and(|value| value.len() > 2 * 1024) {
            return Err(DocError::Schema(
                "command outcome metadata exceeds bounds".into(),
            ));
        }
        let commands = self.doc.get_list("commands");
        for i in 0..commands.len() {
            if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                commands.get(i)
            {
                let id_matches = matches!(
                    map.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == command_id
                );
                if id_matches {
                    map.insert(
                        "status",
                        serde_json::to_value(status)?
                            .as_str()
                            .ok_or_else(|| DocError::Schema("status not a string".into()))?,
                    )?;
                    if let Some(r) = resolution {
                        map.insert("resolution", r)?;
                    }
                    self.doc.commit()?;
                    return Ok(());
                }
            }
        }
        Err(DocError::Schema(format!("command {command_id} not found")))
    }

    /// Stamp a terminal status on an existing message entry by id (recovery:
    /// abandoned `streaming` entries from a dead run are stamped `aborted`).
    /// Returns `false` when no entry with that id exists.
    pub fn set_message_status(
        &self,
        message_id: &str,
        status: MessageStatus,
    ) -> Result<bool, DocError> {
        let _operation = self.doc.operation();
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                messages.get(i)
            {
                let id_matches = matches!(
                    map.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == message_id
                );
                if id_matches {
                    map.insert("status", status_str(status))?;
                    self.doc.commit()?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Append an error part to an existing entry (crash recovery: the aborted
    /// entry must SAY why it ended — "Run interrupted by engine restart…" —
    /// not just truncate silently). Returns `false` when no entry matches.
    pub fn append_error_part(
        &self,
        message_id: &str,
        part_id: &str,
        message: &str,
    ) -> Result<bool, DocError> {
        let _operation = self.doc.operation();
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let id_matches = matches!(
                entry.get("id"),
                Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == message_id
            );
            if !id_matches {
                continue;
            }
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                continue;
            };
            // Idempotent per part id (recovery may re-run on a crash loop).
            for j in 0..parts.len() {
                if let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) =
                    parts.get(j)
                    && matches!(
                        part.get("id"),
                        Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == part_id
                    )
                {
                    return Ok(true);
                }
            }
            push_part(
                &parts,
                &MessagePart::Error {
                    id: part_id.to_string(),
                    message: message.to_string(),
                },
            )?;
            self.doc.commit()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Mark the input part carrying `request_id` resolved, wherever it lives
    /// (input parts store the request id as their part id). The live-run path
    /// resolves through the entry fold; this direct write is for answers to a
    /// question whose run already died — no fold owns the entry anymore.
    /// Returns `false` when no such part exists.
    pub fn resolve_input(&self, request_id: &str) -> Result<bool, DocError> {
        let _operation = self.doc.operation();
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                continue;
            };
            for j in 0..parts.len() {
                let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) =
                    parts.get(j)
                else {
                    continue;
                };
                let is_input = matches!(
                    part.get("kind"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == "input"
                );
                let id_matches = matches!(
                    part.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == request_id
                );
                if is_input && id_matches {
                    part.insert("resolved", true)?;
                    self.doc.commit()?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Export a snapshot (persistence) — `ExportMode::Snapshot`.
    pub fn export_snapshot(&self) -> Result<Vec<u8>, DocError> {
        self.doc
            .export(ExportMode::Snapshot)
            .map_err(|e| DocError::Schema(e.to_string()))
    }
}

fn parts_are_prefix(old: &[MessagePart], new: &[MessagePart]) -> bool {
    old.len() <= new.len() && old.iter().zip(new).all(|(old, new)| {
        if old == new { return true; }
        match (old, new) {
            (MessagePart::Text { id: a, text: old }, MessagePart::Text { id: b, text: new }) => a == b && new.starts_with(old),
            (MessagePart::Tool { id: a, call: old_call, is_error: old_error, resolved: before },
             MessagePart::Tool { id: b, call: new_call, is_error: new_error, resolved: after }) =>
                a == b && (!*before || (*after && old_error == new_error
                    && (old_call == new_call || match (old_call, new_call) {
                        (comet_proto::ToolCall::Todo { items: old }, comet_proto::ToolCall::Todo { items: new }) =>
                            old.len() <= new.len() && old.iter().zip(new).all(|(old, new)|
                                old.text == new.text && (!old.done || new.done)),
                        _ => false,
                    }))),
            (MessagePart::Input { id: a, request_id: old_id, questions: old, resolved: before },
             MessagePart::Input { id: b, request_id: new_id, questions: new, resolved: after }) =>
                a == b && old_id == new_id && old == new && (!*before || *after),
            (MessagePart::Thinking { id: a, started: old_started, completed: before },
             MessagePart::Thinking { id: b, started: new_started, completed: after }) =>
                a == b && (!*old_started || *new_started) && (!*before || *after),
            _ => false,
        }
    })
}

/// Replay order is not ownership order: an old offline phase may append last.
fn session_anchors(publications: &[PublicationRecord]) -> std::collections::BTreeMap<&str, &comet_proto::AgentSessionRecord> {
    let mut anchors = std::collections::BTreeMap::new();
    for publication in publications {
        let PublicationValue::AgentSession(session) = &publication.value else { continue; };
        if publication.published_by != session.owner_subject { continue; }
        let id = session.session_id.as_str();
        match anchors.get(id).copied() {
            None => { anchors.insert(id, session.as_ref()); }
            Some(current) if newer_session_anchor(current, session) => { anchors.insert(id, session.as_ref()); }
            _ => {}
        }
    }
    anchors
}

fn newer_session_anchor(current: &comet_proto::AgentSessionRecord, next: &comet_proto::AgentSessionRecord) -> bool {
    if same_session_owner(current, next) {
        let current_at = current.updated_at.unwrap_or(current.created_at);
        let next_at = next.updated_at.unwrap_or(next.created_at);
        let terminal_rank = |status| match status {
            Some(comet_proto::SessionStatus::Errored) => 2,
            Some(comet_proto::SessionStatus::Idle) => 1,
            Some(comet_proto::SessionStatus::Working | comet_proto::SessionStatus::AwaitingInput) => 0,
            None => -1,
        };
        return next_at > current_at || (next_at == current_at && terminal_rank(next.status) >= terminal_rank(current.status));
    }
    if current.session_id != next.session_id || current.chat_id != next.chat_id
        || current.owner_subject != next.owner_subject
        || current.source != comet_proto::AgentSessionSource::Scaffold
        || next.source != comet_proto::AgentSessionSource::Scaffold {
        return false;
    }
    let Some((sandbox, epoch)) = comet_proto::parse_scaffold_device_id(&current.owner_device_id) else { return false; };
    let Some((next_sandbox, next_epoch)) = comet_proto::parse_scaffold_device_id(&next.owner_device_id) else { return false; };
    if sandbox != next_sandbox || next_epoch <= epoch { return false; }
    let valid_environment = |session: &comet_proto::AgentSessionRecord, expected_epoch| {
        session.environment.as_ref().is_none_or(|environment| {
            environment.owner_principal == session.owner_subject && matches!(&environment.source,
                comet_proto::SessionEnvironmentSource::Scaffold { sandbox_id, lifecycle_epoch, .. }
                    if sandbox_id == sandbox && lifecycle_epoch.is_none_or(|epoch| epoch == expected_epoch))
        })
    };
    if !valid_environment(current, epoch) || !valid_environment(next, next_epoch) { return false; }
    match (&current.environment, &next.environment) {
        (Some(a), Some(b)) => a.scope == b.scope && a.database_environment == b.database_environment,
        (None, _) => true,
        (Some(_), None) => false,
    }
}

fn same_session_owner(a: &comet_proto::AgentSessionRecord, b: &comet_proto::AgentSessionRecord) -> bool {
    a.session_id == b.session_id && a.chat_id == b.chat_id
        && a.owner_subject == b.owner_subject && a.owner_device_id == b.owner_device_id
        && a.source == b.source && a.environment == b.environment
}
fn write_entry_scalar_fields(map: &LoroMap, entry: &SessionMessageEntry) -> Result<(), DocError> {
    map.insert("id", entry.id.as_str())?;
    map.insert(
        "role",
        match entry.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
        },
    )?;
    map.insert("createdAt", entry.created_at)?;
    map.insert("deviceId", entry.device_id.as_str())?;
    if let Some(status) = entry.status {
        map.insert("status", status_str(status))?;
    }
    if let Some(continuation_of) = &entry.continuation_of {
        map.insert("continuationOf", continuation_of.as_str())?;
    }
    if let Some(peer_message) = &entry.peer_message {
        map.insert(
            "peerMessage",
            loro_value_from_json(&serde_json::to_value(peer_message)?),
        )?;
    }
    Ok(())
}

fn status_str(status: MessageStatus) -> &'static str {
    match status {
        MessageStatus::Streaming => "streaming",
        MessageStatus::Complete => "complete",
        MessageStatus::Aborted => "aborted",
        MessageStatus::Queued => "queued",
        MessageStatus::Steered => "steered",
    }
}

/// JSON positions share the same typed container update discipline in maps and arrays.
enum JsonSlot<'a> {
    Map(&'a LoroMap, &'a str),
    List(&'a LoroList, usize),
}

impl JsonSlot<'_> {
    fn get(&self) -> Option<loro::ValueOrContainer> {
        match self { Self::Map(map, key) => map.get(key), Self::List(list, index) => list.get(*index) }
    }

    fn container<C: loro::ContainerTrait>(&self, child: C) -> Result<C, DocError> {
        Ok(match self {
            Self::Map(map, key) => map.insert_container(key, child)?,
            Self::List(list, index) => {
                if *index < list.len() { list.delete(*index, 1)?; }
                list.insert_container(*index, child)?
            }
        })
    }

    fn scalar(&self, value: LoroValue) -> Result<(), DocError> {
        if matches!(self.get(), Some(loro::ValueOrContainer::Value(current)) if current == value) { return Ok(()); }
        match self {
            Self::Map(map, key) => map.insert(key, value)?,
            Self::List(list, index) => {
                if *index < list.len() { list.delete(*index, 1)?; }
                list.insert(*index, value)?;
            }
        }
        Ok(())
    }
}

/// Keep progressive tool arguments in appendable text instead of retaining a
/// whole scalar JSON copy on every refresh. Deep JSON stays wire-compatible.
fn sync_call_json(slot: JsonSlot<'_>, value: &serde_json::Value) -> Result<(), DocError> {
    match value {
        serde_json::Value::Object(fields) => {
            let map = match slot.get() {
                Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => map,
                _ => slot.container(LoroMap::new())?,
            };
            let removed: Vec<_> = map.keys().filter(|key| !fields.contains_key(key.as_str())).collect();
            for key in removed { map.delete(key.as_str())?; }
            for (key, value) in fields { sync_call_json(JsonSlot::Map(&map, key), value)?; }
        }
        serde_json::Value::Array(values) => {
            let list = match slot.get() {
                Some(loro::ValueOrContainer::Container(loro::Container::List(list))) => list,
                _ => slot.container(LoroList::new())?,
            };
            if list.len() > values.len() { list.delete(values.len(), list.len() - values.len())?; }
            for (index, value) in values.iter().enumerate() { sync_call_json(JsonSlot::List(&list, index), value)?; }
        }
        serde_json::Value::String(value) => {
            let text = match slot.get() {
                Some(loro::ValueOrContainer::Container(loro::Container::Text(text))) => text,
                _ => slot.container(LoroText::new())?,
            };
            let previous = text.to_string();
            if let Some(suffix) = value.strip_prefix(previous.as_str()) {
                if !suffix.is_empty() { text.insert(text.len_unicode(), suffix)?; }
            } else {
                text.update(value, Default::default()).map_err(|error| DocError::Schema(error.to_string()))?;
            }
        }
        _ => slot.scalar(loro_value_from_json(value))?,
    }
    Ok(())
}

/// Append one part map to a parts list; text bodies become LoroText containers.
fn push_part(parts: &LoroList, part: &MessagePart) -> Result<(), DocError> {
    let map = parts.push_container(LoroMap::new())?;
    let doc_part = to_doc_part(part)?;
    map.insert("id", doc_part.id.as_str())?;
    map.insert("kind", doc_part.kind.as_str())?;
    if let Some(text) = &doc_part.text {
        let t = map.insert_container("text", LoroText::new())?;
        t.insert(0, text)?;
    }
    if let Some(call) = &doc_part.call {
        sync_call_json(JsonSlot::Map(&map, "call"), call)?;
    }
    if let Some(is_error) = doc_part.is_error {
        map.insert("isError", is_error)?;
    }
    if let Some(questions) = &doc_part.questions {
        map.insert("questions", loro_value_from_json(questions))?;
    }
    if let Some(resolved) = doc_part.resolved {
        map.insert("resolved", resolved)?;
    }
    if let Some(message) = &doc_part.message {
        map.insert("message", message.as_str())?;
    }
    Ok(())
}

pub(crate) fn entry_from_json(v: serde_json::Value) -> Result<SessionMessageEntry, DocError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawEntry {
        id: String,
        role: MessageRole,
        #[serde(default)]
        parts: Vec<DocPartJson>,
        created_at: i64,
        device_id: String,
        #[serde(default)]
        status: Option<MessageStatus>,
        #[serde(default)]
        continuation_of: Option<String>,
        #[serde(default, deserialize_with = "deserialize_peer_message")]
        peer_message: Option<PeerMessageProvenance>,
    }
    let raw: RawEntry = serde_json::from_value(v)?;
    Ok(SessionMessageEntry {
        id: raw.id,
        role: raw.role,
        parts: raw.parts.into_iter().map(from_doc_part).collect(),
        created_at: raw.created_at,
        device_id: raw.device_id,
        status: raw.status,
        continuation_of: raw.continuation_of,
        peer_message: raw.peer_message,
    })
}

fn scalar_string(map: &LoroMap, key: &str) -> Result<String, DocError> {
    match map.get(key) {
        Some(loro::ValueOrContainer::Value(LoroValue::String(value))) => Ok(value.to_string()),
        _ => Err(DocError::Schema(format!("missing string field {key}"))),
    }
}

fn optional_string(map: &LoroMap, key: &str) -> Option<String> {
    match map.get(key) {
        Some(loro::ValueOrContainer::Value(LoroValue::String(value))) => Some(value.to_string()),
        _ => None,
    }
}

fn scalar_i64(map: &LoroMap, key: &str) -> Result<i64, DocError> {
    match map.get(key) {
        Some(loro::ValueOrContainer::Value(LoroValue::I64(value))) => Ok(value),
        _ => Err(DocError::Schema(format!("missing integer field {key}"))),
    }
}

fn entry_from_map_window(
    map: &LoroMap,
    text_budget: &mut usize,
) -> Result<SessionMessageEntry, DocError> {
    let role = match scalar_string(map, "role")?.as_str() {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System,
        other => return Err(DocError::Schema(format!("unknown message role {other}"))),
    };
    let status = match optional_string(map, "status").as_deref() {
        Some("streaming") => Some(MessageStatus::Streaming),
        Some("complete") => Some(MessageStatus::Complete),
        Some("aborted") => Some(MessageStatus::Aborted),
        Some("queued") => Some(MessageStatus::Queued),
        Some("steered") => Some(MessageStatus::Steered),
        Some(other) => return Err(DocError::Schema(format!("unknown message status {other}"))),
        None => None,
    };
    let parts = match map.get("parts") {
        Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) => {
            read_parts_window(&parts, text_budget)?
        }
        _ => Vec::new(),
    };
    Ok(SessionMessageEntry {
        id: scalar_string(map, "id")?,
        role,
        parts,
        created_at: scalar_i64(map, "createdAt")?,
        device_id: scalar_string(map, "deviceId")?,
        status,
        continuation_of: optional_string(map, "continuationOf"),
        peer_message: map
            .get("peerMessage")
            .and_then(|value| serde_json::from_value(value.get_deep_value().to_json_value()).ok()),
    })
}

fn text_tail(text: &LoroText, budget: usize) -> Result<(String, usize), DocError> {
    let total = text.len_utf8();
    if total <= budget {
        return Ok((text.to_string(), 0));
    }
    let target = total - budget;
    for start in target..=(target + 3).min(total) {
        let Ok(delta) = text.slice_delta(start, total, PosType::Bytes) else {
            continue;
        };
        let body = delta
            .into_iter()
            .filter_map(|delta| match delta {
                TextDelta::Insert { insert, .. } => Some(insert),
                _ => None,
            })
            .collect();
        return Ok((body, start));
    }
    Err(DocError::Schema(
        "could not find UTF-8 boundary for transcript tail".into(),
    ))
}

fn read_parts_window(
    parts: &LoroList,
    text_budget: &mut usize,
) -> Result<Vec<MessagePart>, DocError> {
    let mut reversed = Vec::with_capacity(parts.len());
    for index in (0..parts.len()).rev() {
        let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) = parts.get(index)
        else {
            continue;
        };
        let id = scalar_string(&part, "id")?;
        let kind = scalar_string(&part, "kind")?;
        if kind == "text"
            && let Some(loro::ValueOrContainer::Container(loro::Container::Text(text))) =
                part.get("text")
        {
            let (body, omitted) = text_tail(&text, *text_budget)?;
            *text_budget = text_budget.saturating_sub(body.len());
            reversed.push(if omitted == 0 {
                MessagePart::Text { id, text: body }
            } else {
                MessagePart::TextWindow {
                    id,
                    text: body,
                    omitted_prefix_bytes: omitted,
                }
            });
            continue;
        }
        reversed.push(from_doc_part(serde_json::from_value(
            part.get_deep_value().to_json_value(),
        )?));
    }
    reversed.reverse();
    Ok(reversed)
}

/// Render-time continuation join at the entry level (`joinContinuations` in TS):
/// concatenate continuation entries' parts onto their root, in list order.
pub fn join_continuation_entries(entries: Vec<SessionMessageEntry>) -> Vec<SessionMessageEntry> {
    if !entries.iter().any(|e| e.continuation_of.is_some()) {
        return entries;
    }
    let mut out: Vec<SessionMessageEntry> = Vec::with_capacity(entries.len());
    let mut root_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for entry in entries {
        match &entry.continuation_of {
            Some(root_id) => {
                if let Some(&at) = root_index.get(root_id)
                    && out[at].role == entry.role
                    && (!out[at].is_peer_message() || out[at].peer_message == entry.peer_message)
                {
                    out[at].parts.extend(entry.parts);
                } else {
                    // Orphan or incompatible continuation — preserve its visible content.
                    out.push(entry);
                }
            }
            None => {
                root_index.insert(entry.id.clone(), out.len());
                out.push(entry);
            }
        }
    }
    out
}

/// Incremental streaming writer for one assistant entry.
///
/// Port of comet's `DocSegmentWriter` diff discipline: called with the *folded* parts of the
/// live segment (from `fold_event_into_parts`) at each commit tick, it diffs against what's in
/// the doc and writes only the delta:
/// - trailing text growth → `LoroText` append (RLE-merged),
/// - new parts → pushed,
/// - tool call refresh / resolution / input resolution → in-place map updates.
///
/// Invariant relied upon: the fold only ever APPENDS parts or grows the trailing text; earlier
/// text never mutates. Tool/input parts may update fields in place.
pub struct SegmentWriter<'a> {
    doc: &'a SessionDoc,
    /// Stable identity plus an index hint, validated on every write after adoption.
    entry_id: String,
    entry_index: usize,
    generation: u64,
    /// Mirror of what we've written so far (part id → app part).
    written: Vec<MessagePart>,
}

impl<'a> SegmentWriter<'a> {
    /// Begin a streaming assistant entry: pushes the entry with `status: streaming`, no parts.
    pub fn begin(
        doc: &'a SessionDoc,
        entry_id: &str,
        device_id: &str,
        created_at: i64,
    ) -> Result<Self, DocError> {
        let _operation = doc.doc.operation();
        let messages = doc.doc.get_list("messages");
        let entry_index = messages.len();
        let map = messages.push_container(LoroMap::new())?;
        write_entry_scalar_fields(
            &map,
            &SessionMessageEntry {
                id: entry_id.into(),
                role: MessageRole::Assistant,
                parts: vec![],
                created_at,
                device_id: device_id.into(),
                status: Some(MessageStatus::Streaming),
                continuation_of: None,
                peer_message: None,
            },
        )?;
        map.insert_container("parts", LoroList::new())?;
        doc.doc.commit()?;
        Ok(Self {
            doc,
            entry_id: entry_id.to_string(),
            entry_index,
            generation: doc.doc.generation(),
            written: Vec::new(),
        })
    }

    fn entry_map(&self) -> Result<LoroMap, DocError> {
        let messages = self.doc.doc.get_list("messages");
        let matches_id = |map: &LoroMap| matches!(map.get("id"),
            Some(loro::ValueOrContainer::Value(LoroValue::String(id))) if id.as_str() == self.entry_id);
        if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) = messages.get(self.entry_index)
            && matches_id(&map) {
            return Ok(map);
        }
        for index in (0..messages.len()).rev() {
            if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) = messages.get(index)
                && matches_id(&map) {
                return Ok(map);
            }
        }
        Err(DocError::Schema("streaming entry map missing".into()))
    }

    fn parts_list(&self) -> Result<LoroList, DocError> {
        match self.entry_map()?.get("parts") {
            Some(loro::ValueOrContainer::Container(loro::Container::List(list))) => Ok(list),
            _ => Err(DocError::Schema(
                "streaming entry parts list missing".into(),
            )),
        }
    }

    /// Diff `folded` (the full folded segment so far) into the doc.
    pub fn sync(&mut self, folded: &[MessagePart]) -> Result<(), DocError> {
        let binding = self.doc.binding();
        let _operation = binding.operation();
        let generation = binding.generation();
        if generation != self.generation {
            let current = entry_from_json(self.entry_map()?.get_deep_value().to_json_value())?;
            if current.status != Some(MessageStatus::Streaming) || !parts_are_prefix(&current.parts, folded) {
                return Err(DocError::Schema("adopted streaming entry has a terminal or divergent outcome".into()));
            }
            self.written = current.parts;
            self.generation = generation;
        }
        let parts = self.parts_list()?;
        let mut dirty = false;

        for (i, part) in folded.iter().enumerate() {
            match self.written.get(i) {
                None => {
                    push_part(&parts, part)?;
                    self.written.push(part.clone());
                    dirty = true;
                }
                Some(prev) if prev == part => {}
                Some(prev) => {
                    match (prev, part) {
                        (
                            MessagePart::Text { text: old, .. },
                            MessagePart::Text { text: new, .. },
                        ) if new.starts_with(old.as_str()) => {
                            // Trailing-text growth: append the suffix into the LoroText.
                            let delta = &new[old.len()..];
                            if !delta.is_empty() {
                                let part_map = part_map_at(&parts, i)?;
                                match part_map.get("text") {
                                    Some(loro::ValueOrContainer::Container(
                                        loro::Container::Text(t),
                                    )) => {
                                        let len = t.len_unicode();
                                        t.insert(len, delta)?;
                                    }
                                    _ => {
                                        return Err(DocError::Schema(
                                            "text part missing LoroText".into(),
                                        ));
                                    }
                                }
                                dirty = true;
                            }
                        }
                        _ => {
                            // Field-level update (tool refresh/resolve, input resolve, or a
                            // non-append text rewrite, which the fold shouldn't produce —
                            // rewrite the part map fields defensively).
                            let part_map = part_map_at(&parts, i)?;
                            update_part_fields(&part_map, part)?;
                            dirty = true;
                        }
                    }
                    self.written[i] = part.clone();
                }
            }
        }

        if dirty {
            self.doc.doc.commit()?;
        }
        Ok(())
    }

    /// Finish the stream: sync final parts and stamp a terminal status.
    pub fn finish(mut self, folded: &[MessagePart], status: MessageStatus) -> Result<(), DocError> {
        self.sync(folded)?;
        let _operation = self.doc.doc.operation();
        let map = self.entry_map()?;
        if optional_string(&map, "status").as_deref() != Some("streaming") {
            return Err(DocError::Schema("streaming entry already has a terminal outcome".into()));
        }
        map.insert("status", status_str(status))?;
        self.doc.doc.commit()?;
        Ok(())
    }
}

fn part_map_at(parts: &LoroList, index: usize) -> Result<LoroMap, DocError> {
    match parts.get(index) {
        Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => Ok(map),
        _ => Err(DocError::Schema(format!("part map missing at {index}"))),
    }
}

/// In-place field refresh for tool/input parts (and defensive text rewrite).
fn update_part_fields(map: &LoroMap, part: &MessagePart) -> Result<(), DocError> {
    let doc_part = to_doc_part(part)?;
    if let Some(call) = &doc_part.call {
        sync_call_json(JsonSlot::Map(map, "call"), call)?;
    }
    if let Some(is_error) = doc_part.is_error {
        map.insert("isError", is_error)?;
    }
    if let Some(questions) = &doc_part.questions {
        map.insert("questions", loro_value_from_json(questions))?;
    }
    if let Some(resolved) = doc_part.resolved {
        map.insert("resolved", resolved)?;
    }
    if let Some(message) = &doc_part.message {
        map.insert("message", message.as_str())?;
    }
    if let Some(text) = &doc_part.text {
        // Defensive path only — the fold never rewrites earlier text.
        if let Some(loro::ValueOrContainer::Container(loro::Container::Text(t))) = map.get("text") {
            t.update(text, Default::default())
                .map_err(|e| DocError::Schema(e.to_string()))?;
        }
    }
    Ok(())
}

fn loro_value_from_json(v: &serde_json::Value) -> LoroValue {
    LoroValue::from(v.clone())
}

/// Tail sidecar shape (`SessionTail` in TS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTail {
    pub chat_id: String,
    pub schema_version: u32,
    pub messages: Vec<SessionMessageEntry>,
    pub total_messages: usize,
    pub updated_at: i64,
}

/// Materialize the last-N joined messages (`materializeTail` in TS).
pub fn materialize_tail(
    doc: &SessionDoc,
    now: i64,
    tail_count: usize,
) -> Result<SessionTail, DocError> {
    let all = join_continuation_entries(doc.read_entries()?);
    let total = all.len();
    let start = total.saturating_sub(if tail_count == 0 {
        TAIL_MESSAGE_COUNT
    } else {
        tail_count
    });
    Ok(SessionTail {
        chat_id: doc.chat_id().unwrap_or_default(),
        schema_version: SESSION_SCHEMA_VERSION,
        messages: all[start..].to_vec(),
        total_messages: total,
        updated_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parts::fold_event_into_parts;
    use comet_proto::{AgentEvent, ToolCall};

    fn user_entry(id: &str, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 1,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            peer_message: None,
        }
    }

    #[test]
    fn progressive_300k_tool_command_exports_only_linear_updates() {
        const BYTES: usize = 300 * 1024;
        let mut state = 0x1234_5678_u32;
        let command: String = (0..BYTES).map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from(32 + ((state >> 24) % 95) as u8)
        }).collect();
        let doc = SessionDoc::init("chat").unwrap();
        let mut writer = SegmentWriter::begin(&doc, "active", "device", 1).unwrap();
        let mut exported = 0;
        let part = |text: &str| MessagePart::Tool { id: "exec".into(),
            call: ToolCall::Exec { command: text.into() }, is_error: false, resolved: false };
        for end in (1024..=BYTES).step_by(1024) {
            let before = doc.doc().oplog_vv();
            writer.sync(&[part(&command[..end])]).unwrap();
            exported += doc.doc().export(ExportMode::updates(&before)).unwrap().len();
        }
        let expected = part(&command);
        writer.finish(std::slice::from_ref(&expected), MessageStatus::Complete).unwrap();
        assert_eq!(doc.read_entry("active").unwrap().unwrap().parts, vec![expected]);
        // Sum independent deltas: whole-value rewrites cannot hide their
        // quadratic growth through compression against earlier prefixes.
        assert!(exported < BYTES * 3, "progressive tool arguments exported {exported} bytes for {BYTES} input bytes");
    }

    #[test]
    fn mixed_scalar_and_container_tool_peers_preserve_typed_json() {
        let doc = SessionDoc::init("chat").unwrap();
        let mut writer = SegmentWriter::begin(&doc, "active", "device", 1).unwrap();
        let part = |input| MessagePart::Tool { id: "tool".into(),
            call: ToolCall::Unknown { name: "Eval".into(), input: Some(input) }, is_error: false, resolved: false };
        let initial = part(serde_json::json!({
            "nested": { "code": "α" }, "args": [{ "text": "β" }, null, true, 42, -3.5], "removed": "gone",
        }));
        writer.sync(std::slice::from_ref(&initial)).unwrap();
        assert_eq!(doc.read_entry_window(None, 1).unwrap().entries[0].parts, vec![initial]);

        // An older peer replaces the call with the original scalar JSON shape.
        let peer = LoroDoc::new();
        peer.import(&doc.export_snapshot().unwrap()).unwrap();
        let legacy = part(serde_json::json!({ "nested": false, "args": [null, 1], "old": "scalar" }));
        let MessagePart::Tool { call, .. } = &legacy else { unreachable!() };
        let raw_entry = match peer.get_list("messages").get(0) {
            Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => map,
            _ => panic!("entry missing"),
        };
        let raw_parts = match raw_entry.get("parts") {
            Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) => parts,
            _ => panic!("parts missing"),
        };
        part_map_at(&raw_parts, 0).unwrap().insert("call", loro_value_from_json(&serde_json::to_value(call).unwrap())).unwrap();
        peer.commit();
        doc.binding().import(&peer.export(ExportMode::updates(&doc.doc().oplog_vv())).unwrap()).unwrap();
        assert_eq!(doc.read_entry("active").unwrap().unwrap().parts, vec![legacy]);

        let final_part = part(serde_json::json!({
            "nested": { "code": "αβ" }, "args": [{ "text": "βγ" }, false, null, -42, 0.125],
            "emptyObject": {}, "emptyArray": [], "nothing": null,
        }));
        let before = peer.oplog_vv();
        writer.finish(std::slice::from_ref(&final_part), MessageStatus::Complete).unwrap();
        peer.import(&doc.doc().export(ExportMode::updates(&before)).unwrap()).unwrap();
        assert_eq!(doc.read_entry_window(None, 1).unwrap().entries[0].parts, vec![final_part.clone()]);
        assert_eq!(SessionDoc::from_doc(peer).read_entry("active").unwrap().unwrap().parts, vec![final_part]);
    }

    #[test]
    fn live_writer_follows_adopted_snapshot_by_message_id() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let binding = doc.binding();
        binding.install_journal(Vec::new(), std::sync::Arc::new(|_, _| Ok(())));
        let text = |value: &str| vec![MessagePart::Text { id: "text".into(), text: value.into() }];
        let mut writer = SegmentWriter::begin(&doc, "active", "dev-a", 2).unwrap();
        writer.sync(&text("first")).unwrap();
        let replacement = SessionDoc::init("chat-1").unwrap();
        replacement.push_message(&user_entry("prefix", "server history")).unwrap();
        let mut remote = SegmentWriter::begin(&replacement, "active", "dev-a", 2).unwrap();
        remote.sync(&text("fir")).unwrap();
        drop(remote);
        binding.adopt_snapshot(&replacement.export_snapshot().unwrap(), Some("chat-1")).unwrap();
        writer.finish(&text("first continued"), MessageStatus::Complete).unwrap();
        let active = doc.read_entry("active").unwrap().unwrap();
        assert_eq!(active.parts, text("first continued"));
        assert_eq!(active.status, Some(MessageStatus::Complete));
        assert_eq!(doc.read_entry("prefix").unwrap(), Some(user_entry("prefix", "server history")));
        assert_eq!(doc.read_entries().unwrap().iter().filter(|entry| entry.id == "active").count(), 1);
    }

    #[test]
    fn authoritative_backfill_keeps_acknowledged_stream_content_ahead_of_checkpoint() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let binding = doc.binding();
        binding.install_journal(Vec::new(), std::sync::Arc::new(|_, _| Ok(())));
        let text = |value: &str| vec![MessagePart::Text { id: "text".into(), text: value.into() }];
        let mut writer = SegmentWriter::begin(&doc, "active", "dev-a", 2).unwrap();
        writer.sync(&text("first")).unwrap();
        let replacement = SessionDoc::init("chat-1").unwrap();
        let mut remote = SegmentWriter::begin(&replacement, "active", "dev-a", 2).unwrap();
        remote.sync(&text("first acknowledged")).unwrap();
        drop(remote);
        binding.adopt_snapshot(&replacement.export_snapshot().unwrap(), Some("chat-1")).unwrap();
        assert_eq!(doc.read_entry("active").unwrap().unwrap().parts, text("first acknowledged"));
        writer.finish(&text("first acknowledged continued"), MessageStatus::Complete).unwrap();
        assert_eq!(doc.read_entry("active").unwrap().unwrap().parts, text("first acknowledged continued"));
        assert_eq!(doc.read_entries().unwrap().len(), 1);
    }

    #[test]
    fn interrupted_checkpoint_keeps_acknowledged_stream_content() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let binding = doc.binding();
        binding.install_journal(Vec::new(), std::sync::Arc::new(|_, _| Ok(())));
        let text = |value: &str| MessagePart::Text { id: "text".into(), text: value.into() };
        let plan = |done| MessagePart::Tool {
            id: "omp-plan".into(),
            call: ToolCall::Todo { items: vec![comet_proto::TodoItem { text: "Recover the session".into(), done }] },
            is_error: false,
            resolved: true,
        };
        let later = MessagePart::Text { id: "later".into(), text: "later output".into() };
        let error = MessagePart::Error { id: "active-recovery".into(), message: "Run interrupted by Crew restart".into() };
        let writer = SegmentWriter::begin(&doc, "active", "dev-a", 2).unwrap();
        writer.finish(&[text("first"), plan(false), error.clone()], MessageStatus::Aborted).unwrap();
        let replacement = SessionDoc::init("chat-1").unwrap();
        let mut remote = SegmentWriter::begin(&replacement, "active", "dev-a", 2).unwrap();
        remote.sync(&[text("first acknowledged"), plan(true), later.clone()]).unwrap();
        let snapshot = replacement.export_snapshot().unwrap();
        for _ in 0..2 {
            binding.adopt_snapshot(&snapshot, Some("chat-1")).unwrap();
            let adopted = doc.read_entry("active").unwrap().unwrap();
            assert_eq!(adopted.status, Some(MessageStatus::Aborted));
            assert_eq!(adopted.parts, vec![text("first acknowledged"), plan(true), later.clone(), error.clone()]);
        }
        remote.finish(&[text("first acknowledged"), plan(true), later.clone()], MessageStatus::Complete).unwrap();
        binding.adopt_snapshot(&replacement.export_snapshot().unwrap(), Some("chat-1")).unwrap();
        let completed = doc.read_entry("active").unwrap().unwrap();
        assert_eq!(completed.status, Some(MessageStatus::Complete));
        assert_eq!(completed.parts, vec![text("first acknowledged"), plan(true), later]);
        let retained = doc.read_entry("active").unwrap().unwrap();
        let foreign = SessionDoc::init("chat-1").unwrap();
        let mut writer = SegmentWriter::begin(&foreign, "active", "foreign-device", 2).unwrap();
        writer.sync(&[text("first acknowledged")]).unwrap();
        assert!(binding.adopt_snapshot(&foreign.export_snapshot().unwrap(), Some("chat-1")).is_err());
        assert_eq!(doc.read_entry("active").unwrap(), Some(retained));
    }

    #[test]
    fn live_writer_cannot_reopen_adopted_terminal_outcome() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let binding = doc.binding();
        binding.install_journal(Vec::new(), std::sync::Arc::new(|_, _| Ok(())));
        let folded = vec![MessagePart::Text { id: "text".into(), text: "answer".into() }];
        let mut writer = SegmentWriter::begin(&doc, "active", "dev-a", 2).unwrap();
        writer.sync(&folded).unwrap();
        let replacement = SessionDoc::init("chat-1").unwrap();
        let remote = SegmentWriter::begin(&replacement, "active", "dev-a", 2).unwrap();
        remote.finish(&folded, MessageStatus::Aborted).unwrap();
        binding.adopt_snapshot(&replacement.export_snapshot().unwrap(), Some("chat-1")).unwrap();
        assert!(writer.finish(&folded, MessageStatus::Complete).is_err());
        assert_eq!(doc.read_entry("active").unwrap().unwrap().status, Some(MessageStatus::Aborted));
        assert_eq!(doc.read_entry("active").unwrap().unwrap().parts, folded);
    }

    #[test]
    fn replay_respects_terminal_content_and_rejects_divergence() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut entry = user_entry("active", "first");
        entry.role = MessageRole::Assistant;
        entry.status = Some(MessageStatus::Streaming);
        doc.push_message(&entry).unwrap();
        let mut complete = entry.clone();
        complete.parts = vec![MessagePart::Text { id: "t0".into(), text: "first complete".into() }];
        complete.status = Some(MessageStatus::Complete);
        doc.reconcile_message(&complete).unwrap();
        doc.reconcile_message(&entry).unwrap();
        assert_eq!(doc.read_entry("active").unwrap(), Some(complete.clone()));
        let mut divergent = complete.clone();
        divergent.parts = vec![MessagePart::Text { id: "t0".into(), text: "different".into() }];
        assert!(doc.reconcile_message(&divergent).is_err());
        divergent = complete.clone();
        divergent.device_id = "other-owner".into();
        assert!(doc.reconcile_message(&divergent).is_err());
        assert_eq!(doc.read_entry("active").unwrap(), Some(complete));
    }

    #[test]
    fn streaming_replay_refreshes_tools_but_never_regresses_resolution() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut entry = user_entry("active", "");
        entry.role = MessageRole::Assistant;
        entry.status = Some(MessageStatus::Streaming);
        entry.parts = vec![MessagePart::Tool { id: "tool".into(),
            call: ToolCall::Exec { command: "echo".into() }, is_error: false, resolved: false }];
        doc.push_message(&entry).unwrap();
        let mut updated = entry.clone();
        updated.parts = vec![MessagePart::Tool { id: "tool".into(),
            call: ToolCall::Exec { command: "echo done".into() }, is_error: false, resolved: true }];
        doc.reconcile_message(&updated).unwrap();
        assert_eq!(doc.read_entry("active").unwrap(), Some(updated.clone()));
        doc.reconcile_message(&entry).unwrap();
        assert_eq!(doc.read_entry("active").unwrap(), Some(updated));
    }

    #[test]
    fn owner_heartbeats_are_bounded_and_phase_history_stays_durable() {
        use comet_proto::{AgentSessionRecord, AgentSessionSource, SessionStatus};
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut record = PublicationRecord {
            id: "start".into(), schema_version: COLLABORATION_SCHEMA_VERSION,
            published_at: 1, published_by: "owner".into(), unknown: Default::default(),
            value: PublicationValue::AgentSession(Box::new(AgentSessionRecord {
                session_id: "chat-1".into(), chat_id: "chat-1".into(),
                owner_subject: "owner".into(), owner_device_id: "dev-a".into(),
                source: AgentSessionSource::Local, environment: None,
                harness: None, model: None, harness_session_id: None,
                status: Some(SessionStatus::Working), updated_at: Some(1), created_at: 1,
                unknown: Default::default(),
            })),
        };
        doc.append_publication(&record).unwrap();
        let mut heartbeat_base = Vec::new();
        for at in 2..=1_000 {
            if at == 3 { doc.binding().install_journal(Vec::new(), std::sync::Arc::new(|_, _| Ok(()))); }
            record.id = format!("heartbeat-{at}");
            record.published_at = at;
            let PublicationValue::AgentSession(session) = &mut record.value else { unreachable!() };
            session.updated_at = Some(at);
            doc.upsert_agent_session(&record).unwrap();
            if at == 2 { heartbeat_base = doc.export_snapshot().unwrap(); }
        }
        assert_eq!(doc.doc().get_map("agentSessions").len(), 1);
        assert_eq!(doc.read_publications().unwrap().len(), 1);
        assert_eq!(doc.collaboration_snapshot().unwrap().sessions[0].updated_at, Some(1_000));
        let current = doc.export_snapshot().unwrap();
        let raw = LoroDoc::new(); raw.import(&current).unwrap();
        let restored = SessionDoc::from_doc(raw);
        restored.binding().install_journal(doc.binding().pending_records(), std::sync::Arc::new(|_, _| Ok(())));
        restored.binding().adopt_snapshot(&heartbeat_base, Some("chat-1")).unwrap();
        restored.binding().adopt_snapshot(&current, Some("chat-1")).unwrap();
        assert_eq!(restored.collaboration_snapshot().unwrap().sessions[0].updated_at, Some(1_000));
        assert_eq!(restored.doc().get_map("agentSessions").len(), 1);
        record.id = "idle".into();
        record.published_at = 1_000;
        let PublicationValue::AgentSession(session) = &mut record.value else { unreachable!() };
        session.status = Some(SessionStatus::Idle);
        session.updated_at = Some(1_000);
        doc.append_publication(&record).unwrap();
        assert_eq!(doc.collaboration_snapshot().unwrap().sessions[0].status, Some(SessionStatus::Idle),
            "the immutable terminal transition wins a same-millisecond heartbeat tie");
        doc.upsert_agent_session(&record).unwrap();
        let PublicationValue::AgentSession(session) = &mut record.value else { unreachable!() };
        session.owner_device_id = "attacker".into();
        session.updated_at = Some(2_000);
        assert!(doc.upsert_agent_session(&record).is_err());
        assert_eq!(doc.read_publications().unwrap().iter().map(|record| record.id.as_str()).collect::<Vec<_>>(), ["start", "idle"]);
        assert_eq!(doc.collaboration_snapshot().unwrap().sessions[0].status, Some(SessionStatus::Idle));
        assert_eq!(doc.doc().get_map("agentSessions").len(), 1);
    }

    #[test]
    fn replayed_offline_phase_cannot_displace_terminal_or_newer_scaffold_owner() {
        use comet_proto::{AgentSessionRecord, AgentSessionSource, SessionStatus};
        for (source, old_device, current_device, old_at) in [
            (AgentSessionSource::Local, "dev-a", "dev-a", 10),
            (AgentSessionSource::Scaffold, "comet-scaffold-sandbox-e1", "comet-scaffold-sandbox-e2", 10_000),
        ] {
            let publication = |id: &str, device: &str, at, status| PublicationRecord {
                id: id.into(), schema_version: COLLABORATION_SCHEMA_VERSION, published_at: at,
                published_by: "owner".into(), unknown: Default::default(),
                value: PublicationValue::AgentSession(Box::new(AgentSessionRecord {
                    session_id: "chat".into(), chat_id: "chat".into(), owner_subject: "owner".into(),
                    owner_device_id: device.into(), source, environment: None,
                    harness: None, model: None, harness_session_id: None,
                    status: Some(status), updated_at: Some(at), created_at: 1, unknown: Default::default(),
                })),
            };
            let local = SessionDoc::init("chat").unwrap();
            let binding = local.binding();
            binding.install_journal(Vec::new(), std::sync::Arc::new(|_, _| Ok(())));
            let old = publication("offline-old", old_device, old_at, SessionStatus::Working);
            local.append_publication(&old).unwrap();
            let server = SessionDoc::init("chat").unwrap();
            let terminal = publication("current-terminal", current_device, 20, SessionStatus::Idle);
            server.append_publication(&terminal).unwrap();
            server.upsert_agent_session(&publication("heartbeat", current_device, 30, SessionStatus::Idle)).unwrap();
            binding.adopt_snapshot(&server.export_snapshot().unwrap(), Some("chat")).unwrap();
            let snapshot = local.collaboration_snapshot().unwrap();
            assert_eq!(snapshot.sessions[0].owner_device_id, current_device);
            assert_eq!(snapshot.sessions[0].status, Some(SessionStatus::Idle));
            assert_eq!(snapshot.sessions[0].updated_at, Some(30));
            assert_eq!(snapshot.publications.iter().map(|record| record.id.as_str()).collect::<Vec<_>>(), ["current-terminal", "offline-old"]);
            // Heartbeats and projection must use the same selected anchor,
            // even though the obsolete offline phase was appended last.
            local.upsert_agent_session(&publication("next-heartbeat", current_device, 40, SessionStatus::Idle)).unwrap();
            local.append_publication(&publication("same-ms-active", current_device, 20, SessionStatus::Working)).unwrap();
            let mut foreign = publication("foreign-owner", "foreign-device", 50_000, SessionStatus::Working);
            foreign.published_by = "foreign".into();
            let PublicationValue::AgentSession(session) = &mut foreign.value else { unreachable!() };
            session.owner_subject = "foreign".into();
            local.append_publication(&foreign).unwrap();
            let snapshot = local.collaboration_snapshot().unwrap();
            assert_eq!(snapshot.sessions[0].owner_device_id, current_device);
            assert_eq!(snapshot.sessions[0].status, Some(SessionStatus::Idle));
            assert_eq!(snapshot.sessions[0].updated_at, Some(40));
            assert_eq!(snapshot.publications.len(), 4, "obsolete and foreign history remains durable but not authoritative");
            if source == AgentSessionSource::Scaffold {
                local.append_publication(&publication("next-epoch", "comet-scaffold-sandbox-e3", 5, SessionStatus::Idle)).unwrap();
                local.append_publication(&publication("old-epoch-future-clock", current_device, 100_000, SessionStatus::Working)).unwrap();
                local.append_publication(&publication("different-sandbox", "comet-scaffold-other-e99", 200_000, SessionStatus::Working)).unwrap();
                let snapshot = local.collaboration_snapshot().unwrap();
                assert_eq!(snapshot.sessions[0].owner_device_id, "comet-scaffold-sandbox-e3");
                assert_eq!(snapshot.sessions[0].status, Some(SessionStatus::Idle));
                assert_eq!(snapshot.sessions[0].updated_at, Some(5));
                assert!(local.upsert_agent_session(&publication("wrong-old-heartbeat", current_device, 300_000, SessionStatus::Working)).is_err());
                local.upsert_agent_session(&publication("current-heartbeat", "comet-scaffold-sandbox-e3", 50, SessionStatus::Idle)).unwrap();
                assert_eq!(local.collaboration_snapshot().unwrap().sessions[0].updated_at, Some(50));
                assert_eq!(local.read_publications().unwrap().len(), 7);
            }
        }
    }

    fn peer_entry(id: &str, text: &str) -> SessionMessageEntry {
        let mut entry = user_entry(id, text);
        entry.peer_message = Some(PeerMessageProvenance {
            command_id: id.into(),
            source_chat_id: "source-chat".into(),
            thread_id: "thread".into(),
            reply_to: Some("previous-command".into()),
        });
        entry
    }

    fn peer_command(id: &str) -> SessionCommandEntry {
        SessionCommandEntry {
            id: id.into(),
            payload: crate::commands::SessionCommandPayload::PeerMessage {
                text: "original peer text".into(),
                source_chat_id: "source-chat".into(),
                source_deployment_id: None,
                source_device_id: Some("source-device".into()),
                thread_id: "thread".into(),
                reply_to: None,
                hop_count: 0,
            },
            issued_by: "dev-a".into(),
            issued_at: 1,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        }
    }

    #[test]
    fn peer_provenance_survives_snapshot_window_and_tail() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let entry = peer_entry("peer-command", "unchanged original peer text");
        doc.push_message(&entry).unwrap();
        let other = LoroDoc::new();
        other.import(&doc.export_snapshot().unwrap()).unwrap();
        let restored = SessionDoc::from_doc(other);
        assert_eq!(restored.read_entries().unwrap(), vec![entry.clone()]);
        let window = restored.read_entry_window(None, 1).unwrap();
        assert_eq!(window.entries, vec![entry.clone()]);
        assert!(window.entries[0].is_peer_message());
        let tail = materialize_tail(&restored, 2, 1).unwrap();
        let wire = serde_json::to_value(&tail).unwrap();
        assert_eq!(
            wire["messages"][0]["peerMessage"],
            serde_json::json!({
                "commandId": "peer-command",
                "sourceChatId": "source-chat",
                "threadId": "thread",
                "replyTo": "previous-command"
            })
        );
        let decoded: SessionTail = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.messages, vec![entry]);
        assert!(decoded.messages[0].is_peer_message());
    }

    #[test]
    fn peer_provenance_is_independent_of_bounded_text_projection() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let source = "x".repeat(TAIL_TEXT_BYTE_BUDGET + 10);
        let entry = peer_entry("peer-command", &source);
        doc.push_message(&entry).unwrap();
        let window = doc.read_entry_window(None, 1).unwrap();
        assert!(window.entries[0].is_peer_message());
        assert_eq!(window.entries[0].peer_message, entry.peer_message);
        assert_eq!(
            window.entries[0].parts,
            vec![MessagePart::TextWindow {
                id: "t0".into(),
                text: source[10..].into(),
                omitted_prefix_bytes: 10,
            }]
        );
        assert_eq!(
            doc.read_message("peer-command").unwrap(),
            Some(entry.clone())
        );
        assert!(doc.read_message("missing").unwrap().is_none());
        assert_eq!(doc.read_entries().unwrap(), vec![entry]);
    }

    #[test]
    fn old_peer_lookalike_remains_visible_even_with_matching_command() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let entry = user_entry(
            "peer-command",
            "[Message from agent source-chat]\noriginal peer text",
        );
        doc.push_message(&entry).unwrap();
        doc.queue_command(&peer_command("peer-command")).unwrap();
        let other = LoroDoc::new();
        other.import(&doc.export_snapshot().unwrap()).unwrap();
        let restored = SessionDoc::from_doc(other);
        for entries in [
            restored.read_entries().unwrap(),
            restored.read_entry_window(None, 1).unwrap().entries,
            materialize_tail(&restored, 2, 1).unwrap().messages,
        ] {
            assert_eq!(entries, vec![entry.clone()]);
            assert!(!entries[0].is_peer_message());
            assert!(
                serde_json::to_value(&entries[0])
                    .unwrap()
                    .get("peerMessage")
                    .is_none()
            );
        }
    }

    #[test]
    fn malformed_peer_metadata_never_discards_message_content() {
        for metadata in [
            serde_json::json!(null),
            serde_json::json!("peer-command"),
            serde_json::json!({"commandId": "peer-command"}),
            serde_json::json!({"commandId": 7, "sourceChatId": "source", "threadId": "thread"}),
            serde_json::json!({"commandId": "peer-command", "sourceChatId": "source", "threadId": "thread", "replyTo": 7}),
        ] {
            let doc = SessionDoc::init("chat-1").unwrap();
            let entry = user_entry("peer-command", "never discard this text");
            doc.push_message(&entry).unwrap();
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                doc.doc().get_list("messages").get(0)
            else {
                panic!("missing message map");
            };
            map.insert("peerMessage", loro_value_from_json(&metadata))
                .unwrap();
            assert_eq!(doc.read_entries().unwrap(), vec![entry.clone()]);
            assert_eq!(
                doc.read_entry_window(None, 1).unwrap().entries,
                vec![entry.clone()]
            );
            let mut wire = serde_json::to_value(&entry).unwrap();
            wire["peerMessage"] = metadata;
            let decoded: SessionMessageEntry = serde_json::from_value(wire).unwrap();
            assert_eq!(decoded, entry);
            assert!(!decoded.is_peer_message());
        }
    }

    #[test]
    fn peer_visibility_requires_valid_identity_and_user_role() {
        let entry = peer_entry("peer-command", "peer text");
        assert!(entry.is_peer_message());
        for role in [MessageRole::Assistant, MessageRole::System] {
            let mut other = entry.clone();
            other.role = role;
            assert!(!other.is_peer_message());
        }
        for (field, value) in [
            ("commandId", "other-command"),
            ("commandId", " "),
            ("sourceChatId", "\t"),
            ("threadId", ""),
            ("replyTo", "\n"),
        ] {
            let mut wire = serde_json::to_value(&entry).unwrap();
            wire["peerMessage"][field] = value.into();
            let decoded: SessionMessageEntry = serde_json::from_value(wire).unwrap();
            assert!(!decoded.is_peer_message());
            assert_eq!(decoded.parts, entry.parts);
        }
        assert!(!peer_entry(" ", "peer text").is_peer_message());
        let mut wire = serde_json::to_value(&entry).unwrap();
        wire["peerMessage"]["replyTo"] = serde_json::Value::Null;
        wire["peerMessage"]["futureField"] = true.into();
        let decoded: SessionMessageEntry = serde_json::from_value(wire).unwrap();
        assert!(decoded.is_peer_message());
        assert!(
            serde_json::to_value(decoded).unwrap()["peerMessage"]
                .get("replyTo")
                .is_none()
        );
    }

    #[test]
    fn continuation_join_keeps_unprovenanced_and_assistant_content_visible() {
        let root = peer_entry("peer-command", "root peer text");
        let mut continuation = user_entry("peer-command#c1", "continued peer text");
        continuation.continuation_of = Some(root.id.clone());
        continuation.peer_message = root.peer_message.clone();
        let mut ordinary = user_entry("ordinary", "ordinary content");
        ordinary.continuation_of = Some(root.id.clone());
        let mut assistant = peer_entry("assistant", "assistant answer");
        assistant.role = MessageRole::Assistant;
        assistant.continuation_of = Some(root.id.clone());
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_messages(&[
            root.clone(),
            continuation.clone(),
            ordinary.clone(),
            assistant.clone(),
        ])
        .unwrap();
        let mut joined_root = root;
        joined_root.parts.extend(continuation.parts.clone());
        for entries in [
            join_continuation_entries(doc.read_entries().unwrap()),
            doc.read_entry_window(None, 1).unwrap().entries,
            materialize_tail(&doc, 2, 3).unwrap().messages,
        ] {
            assert_eq!(
                entries,
                vec![joined_root.clone(), ordinary.clone(), assistant.clone()]
            );
            assert!(entries[0].is_peer_message());
            assert!(!entries[1].is_peer_message());
            assert!(!entries[2].is_peer_message());
        }
        assert_eq!(
            doc.read_message("peer-command").unwrap(),
            Some(joined_root.clone())
        );
        assert_eq!(doc.read_message("ordinary").unwrap(), Some(ordinary));
        // Even a self-matching orphan must not become hidden without its root.
        continuation.peer_message.as_mut().unwrap().command_id = continuation.id.clone();
        let orphan = join_continuation_entries(vec![continuation.clone()]);
        assert_eq!(orphan, vec![continuation]);
        assert!(!orphan[0].is_peer_message());
        // Child metadata cannot retroactively classify an ordinary root.
        let mut ordinary_root = joined_root;
        ordinary_root.peer_message = None;
        let joined = join_continuation_entries(vec![ordinary_root, orphan[0].clone()]);
        assert!(!joined[0].is_peer_message());
    }

    #[test]
    fn exact_command_lookup_skips_malformed_rows_without_guessing() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let malformed = doc
            .doc()
            .get_list("commands")
            .push_container(LoroMap::new())
            .unwrap();
        malformed.insert("id", "peer-command").unwrap();
        doc.queue_command(&peer_command("peer-command-other"))
            .unwrap();
        assert!(doc.read_command("peer-command").unwrap().is_none());
        let command = peer_command("peer-command");
        doc.queue_command(&command).unwrap();
        assert_eq!(doc.read_command("peer-command").unwrap(), Some(command));
        assert!(doc.read_command("peer").unwrap().is_none());
    }

    #[test]
    fn exact_entry_lookup_preserves_original_provenance_without_backfill() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let malformed = doc
            .doc()
            .get_list("messages")
            .push_container(LoroMap::new())
            .unwrap();
        malformed.insert("id", "peer-command").unwrap();
        doc.push_message(&peer_entry("peer-command-other", "unrelated"))
            .unwrap();
        assert!(doc.read_entry("peer-command").unwrap().is_none());
        let entry = peer_entry("peer-command", "original");
        doc.push_message(&entry).unwrap();
        assert_eq!(doc.read_entry("peer-command").unwrap(), Some(entry));
        assert!(doc.read_entry("peer").unwrap().is_none());
        let historical = user_entry("historical-command", "original peer text");
        doc.push_message(&historical).unwrap();
        doc.queue_command(&peer_command("historical-command"))
            .unwrap();
        assert_eq!(
            doc.read_entry("historical-command").unwrap(),
            Some(historical)
        );
    }

    #[test]
    fn round_trips_message_entries() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("m1", "hello")).unwrap();
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "m1");
        assert_eq!(
            entries[0].parts,
            vec![MessagePart::Text {
                id: "t0".into(),
                text: "hello".into()
            }]
        );
        assert_eq!(doc.chat_id().as_deref(), Some("chat-1"));
    }

    #[test]
    fn resolve_input_stamps_the_part_in_place() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Input {
                id: "r1".into(),
                request_id: "r1".into(),
                questions: vec![],
                resolved: false,
            }],
            created_at: 1,
            device_id: "dev-a".into(),
            // The orphan case: the run died and recovery stamped the entry.
            status: Some(MessageStatus::Aborted),
            continuation_of: None,
            peer_message: None,
        })
        .unwrap();
        assert!(!doc.resolve_input("nope").unwrap());
        assert!(doc.resolve_input("r1").unwrap());
        let entries = doc.read_entries().unwrap();
        assert!(matches!(
            &entries[0].parts[0],
            MessagePart::Input { resolved: true, .. }
        ));
    }

    #[test]
    fn snapshot_round_trips_between_docs() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("m1", "hello")).unwrap();
        let bytes = doc.export_snapshot().unwrap();

        let other = LoroDoc::new();
        other.import(&bytes).unwrap();
        let restored = SessionDoc::from_doc(other);
        assert_eq!(
            restored.read_entries().unwrap(),
            doc.read_entries().unwrap()
        );
    }

    #[test]
    fn two_peers_converge_on_concurrent_inserts() {
        let a = SessionDoc::init("chat-1").unwrap();
        let b = SessionDoc::from_doc({
            let d = LoroDoc::new();
            d.import(&a.export_snapshot().unwrap()).unwrap();
            d
        });
        a.push_message(&user_entry("m-a", "from a")).unwrap();
        b.push_message(&user_entry("m-b", "from b")).unwrap();

        // Cross-import updates.
        let a_update = a
            .doc()
            .export(ExportMode::updates(&b.doc().oplog_vv()))
            .unwrap();
        let b_update = b
            .doc()
            .export(ExportMode::updates(&a.doc().oplog_vv()))
            .unwrap();
        b.doc().import(&a_update).unwrap();
        a.doc().import(&b_update).unwrap();

        let ea = a.read_entries().unwrap();
        let eb = b.read_entries().unwrap();
        assert_eq!(ea, eb);
        assert_eq!(ea.len(), 2); // one insert from each peer, converged in the same order
    }

    #[test]
    fn segment_writer_streams_text_incrementally() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut writer = SegmentWriter::begin(&doc, "a1", "dev-a", 5).unwrap();

        let mut folded = Vec::new();
        fold_event_into_parts(&mut folded, &AgentEvent::TextDelta { text: "Hel".into() });
        writer.sync(&folded).unwrap();
        fold_event_into_parts(&mut folded, &AgentEvent::TextDelta { text: "lo".into() });
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolCall {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolResult {
                id: "tool-1".into(),
                is_error: false,
                output: None,
            },
        );
        writer.sync(&folded).unwrap();
        writer.finish(&folded, MessageStatus::Complete).unwrap();

        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, Some(MessageStatus::Complete));
        assert_eq!(entries[0].parts.len(), 2);
        match &entries[0].parts[0] {
            MessagePart::Text { text, .. } => assert_eq!(text, "Hello"),
            other => panic!("unexpected {other:?}"),
        }
        match &entries[0].parts[1] {
            MessagePart::Tool {
                resolved, is_error, ..
            } => {
                assert!(*resolved);
                assert!(!*is_error);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn set_message_status_stamps_existing_entry() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut entry = user_entry("m1", "hello");
        entry.role = MessageRole::Assistant;
        entry.status = Some(MessageStatus::Streaming);
        doc.push_message(&entry).unwrap();

        assert!(
            doc.set_message_status("m1", MessageStatus::Aborted)
                .unwrap()
        );
        assert!(
            !doc.set_message_status("nope", MessageStatus::Aborted)
                .unwrap()
        );
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries[0].status, Some(MessageStatus::Aborted));
    }

    #[test]
    fn command_queue_and_outcome_round_trip() {
        use crate::commands::{SessionCommandPayload, SessionCommandStatus};
        let doc = SessionDoc::init("chat-1").unwrap();
        let entry = SessionCommandEntry {
            id: "c1".into(),
            payload: SessionCommandPayload::Steer {
                prompt: "focus".into(),
                message_id: None,
            },
            issued_by: "dev-b".into(),
            issued_at: 10,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        doc.queue_command(&entry).unwrap();
        doc.set_command_status("c1", SessionCommandStatus::Applied, None)
            .unwrap();
        let commands = doc.read_commands().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, SessionCommandStatus::Applied);
        assert_eq!(commands[0].payload, entry.payload);
    }

    #[test]
    fn command_queue_is_idempotent_by_entry_id() {
        use crate::commands::{SessionCommandPayload, SessionCommandStatus};
        let doc = SessionDoc::init("chat-1").unwrap();
        let first = SessionCommandEntry {
            id: "stable-id".into(),
            payload: SessionCommandPayload::Interrupt {},
            issued_by: "dev-a".into(),
            issued_at: 10,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        let retry = SessionCommandEntry {
            payload: SessionCommandPayload::Steer {
                prompt: "must not replace the original".into(),
                message_id: None,
            },
            ..first.clone()
        };
        doc.queue_command(&first).unwrap();
        doc.queue_command(&retry).unwrap();
        let commands = doc.read_commands().unwrap();
        assert_eq!(commands, vec![first]);
    }

    #[test]
    fn unknown_publication_kinds_do_not_enter_collaboration_state() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let record = PublicationRecord {
            id: "future-1".into(),
            schema_version: COLLABORATION_SCHEMA_VERSION,
            published_at: 1,
            published_by: "iap:alice@example.com".into(),
            value: PublicationValue::Unknown {
                kind: "futureControl".into(),
                value: serde_json::json!({ "action": "mutate" }),
            },
            unknown: Default::default(),
        };

        assert!(doc.append_publication(&record).is_err());
        assert!(doc.read_publications().unwrap().is_empty());
    }

    #[test]
    fn tail_materializes_last_n_joined() {
        let doc = SessionDoc::init("chat-1").unwrap();
        for i in 0..5 {
            doc.push_message(&user_entry(&format!("m{i}"), &format!("msg {i}")))
                .unwrap();
        }
        let tail = materialize_tail(&doc, 99, 2).unwrap();
        assert_eq!(tail.total_messages, 5);
        assert_eq!(tail.messages.len(), 2);
        assert_eq!(tail.messages[1].id, "m4");
        assert_eq!(tail.chat_id, "chat-1");
    }

    #[test]
    fn entry_window_pages_from_tail_without_overlap() {
        let doc = SessionDoc::init("chat-1").unwrap();
        for i in 0..5 {
            doc.push_message(&user_entry(&format!("m{i}"), &format!("msg {i}")))
                .unwrap();
        }

        let tail = doc.read_entry_window(None, 2).unwrap();
        assert_eq!(
            tail.entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["m3", "m4"]
        );
        assert_eq!(tail.before, Some(3));

        let older = doc.read_entry_window(tail.before, 2).unwrap();
        assert_eq!(
            older
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["m1", "m2"]
        );
        assert_eq!(older.before, Some(1));

        let oldest = doc.read_entry_window(older.before, 2).unwrap();
        assert_eq!(
            oldest
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["m0"]
        );
        assert_eq!(oldest.before, None);
    }

    #[test]
    fn entry_window_truncates_oversized_utf8_text_at_a_boundary() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let source = format!("{}END", "é".repeat(TAIL_TEXT_BYTE_BUDGET / 2 + 8));
        doc.push_message(&user_entry("large", &source)).unwrap();

        let window = doc.read_entry_window(None, 1).unwrap();
        let MessagePart::TextWindow {
            text,
            omitted_prefix_bytes,
            ..
        } = &window.entries[0].parts[0]
        else {
            panic!("oversized text should be projected as a bounded window");
        };
        assert!(*omitted_prefix_bytes > 0);
        assert!(source.is_char_boundary(*omitted_prefix_bytes));
        assert_eq!(text, &source[*omitted_prefix_bytes..]);
        assert!(text.len() <= TAIL_TEXT_BYTE_BUDGET);
        assert!(text.ends_with("END"));
    }

    #[test]
    fn entry_window_includes_continuations_with_their_root() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("older", "older")).unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "root".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "first".into(),
            }],
            created_at: 2,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            peer_message: None,
        })
        .unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "root#c1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "t1".into(),
                text: "second".into(),
            }],
            created_at: 2,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: Some("root".into()),
            peer_message: None,
        })
        .unwrap();

        let tail = doc.read_entry_window(None, 1).unwrap();
        assert_eq!(tail.entries.len(), 1);
        assert_eq!(tail.entries[0].id, "root");
        assert_eq!(tail.entries[0].parts.len(), 2);
        assert_eq!(tail.before, Some(1));
    }
}
