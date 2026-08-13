//! Delta frames for `WatchDocMessages`.
//!
//! The watch used to re-serialize the entire transcript on every 120ms commit
//! tick (measured at 1.13MB per frame on a 1.6MB chat, ~4 copies deep through
//! the RPC hop). A frame is now either a full `reset` (first frame, and the
//! fallback when a diff would approach transcript size) or the changed
//! entries only — during streaming that is one entry per tick.
//!
//! Both viewports share this module (the `comet_proto::view` rule: derivations
//! that must not diverge per surface live in one place).

use serde::{Deserialize, Serialize};

use crate::parts::MessagePart;
use crate::schema::SessionMessageEntry;

/// One `WatchDocMessages` stream item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TranscriptFrame {
    Reset {
        reset: Vec<SessionMessageEntry>,
        /// Raw-list cursor for the next older page; `None` means the window
        /// already reaches the beginning of the transcript.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<usize>,
    },
    Delta {
        #[serde(default)]
        upsert: Vec<TranscriptUpsert>,
        #[serde(default)]
        append: Vec<TextAppend>,
        #[serde(default)]
        remove: Vec<String>,
        /// Expected transcript length after applying this frame — the desync
        /// tripwire: a consumer that lands elsewhere resubscribes for a reset.
        count: usize,
        /// Updated raw-list cursor for the next older page. A tail append can
        /// retire the oldest visible row and advance this cursor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<usize>,
    },
}

/// An inserted or replaced entry, positioned after `after` (`None` = head).
/// Anchors are prior upserts of the same frame or unchanged entries, so
/// applying upserts in frame order always finds them settled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptUpsert {
    pub after: Option<String>,
    pub entry: SessionMessageEntry,
}

/// A pure text-tail append to one part — the streaming hot path. Entry-level
/// upserts re-send the whole live entry per tick, which for a long single
/// reply is the whole reply again (continuations re-join before the watch);
/// this carries only the new tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextAppend {
    pub entry: String,
    pub part: String,
    pub text: String,
    /// Bytes removed from the visible text prefix before appending `text`.
    #[serde(default)]
    pub drop_prefix: usize,
    /// Omitted source bytes represented by the resulting projected part.
    #[serde(default)]
    pub omitted_prefix_bytes: usize,
    /// Total visible text length after applying this append.
    pub len: usize,
}

impl TranscriptFrame {
    pub fn reset(entries: &[SessionMessageEntry], before: Option<usize>) -> Self {
        Self::Reset {
            reset: entries.to_vec(),
            before,
        }
    }

    pub fn before(&self) -> Option<usize> {
        match self {
            Self::Reset { before, .. } | Self::Delta { before, .. } => *before,
        }
    }

    pub fn reset_entries(&self) -> Option<&[SessionMessageEntry]> {
        match self {
            Self::Reset { reset, .. } => Some(reset),
            Self::Delta { .. } => None,
        }
    }

    pub fn removed_entry_ids(&self) -> &[String] {
        match self {
            Self::Reset { .. } => &[],
            Self::Delta { remove, .. } => remove,
        }
    }

    pub fn is_empty_delta(&self) -> bool {
        matches!(
            self,
            Self::Delta { upsert, append, remove, .. }
                if upsert.is_empty() && append.is_empty() && remove.is_empty()
        )
    }
}

/// When `next` is `prev` plus text appended to exactly one text part (same
/// position, all other fields and parts identical), the change is a
/// [`TextAppend`]. Any other difference falls back to a full upsert.
fn try_text_append(prev: &SessionMessageEntry, next: &SessionMessageEntry) -> Option<TextAppend> {
    if prev.id != next.id
        || prev.role != next.role
        || prev.created_at != next.created_at
        || prev.device_id != next.device_id
        || prev.status != next.status
        || prev.continuation_of != next.continuation_of
        || prev.parts.len() != next.parts.len()
    {
        return None;
    }
    let mut append = None;
    for (previous, current) in prev.parts.iter().zip(&next.parts) {
        if previous == current {
            continue;
        }
        let (pid, previous_text, previous_omitted) = text_projection(previous)?;
        let (nid, current_text, current_omitted) = text_projection(current)?;
        if pid != nid || current_omitted < previous_omitted || append.is_some() {
            return None;
        }
        let drop_prefix = current_omitted - previous_omitted;
        if drop_prefix > previous_text.len() {
            return None;
        }
        let overlap = previous_text.len() - drop_prefix;
        if overlap > current_text.len()
            || previous_text.as_bytes()[drop_prefix..] != current_text.as_bytes()[..overlap]
        {
            return None;
        }
        append = Some(TextAppend {
            entry: next.id.clone(),
            part: nid.to_string(),
            text: current_text[overlap..].to_string(),
            drop_prefix,
            omitted_prefix_bytes: current_omitted,
            len: current_text.len(),
        });
    }
    append
}

fn text_projection(part: &MessagePart) -> Option<(&str, &str, usize)> {
    match part {
        MessagePart::Text { id, text } => Some((id, text, 0)),
        MessagePart::TextWindow {
            id,
            text,
            omitted_prefix_bytes,
        } => Some((id, text, *omitted_prefix_bytes)),
        _ => None,
    }
}

/// Diff two transcript states into a frame. An entry is upserted when it is
/// new, its content changed, or its predecessor changed (a Loro list merge can
/// interleave a remote entry mid-list). Falls back to `Reset` when the delta
/// would carry most of the transcript anyway.
pub fn diff_transcript(
    prev: &[SessionMessageEntry],
    next: &[SessionMessageEntry],
    before: Option<usize>,
) -> TranscriptFrame {
    let mut prev_by_id =
        std::collections::HashMap::<&str, (usize, &SessionMessageEntry)>::with_capacity(prev.len());
    for (index, entry) in prev.iter().enumerate() {
        if prev_by_id
            .insert(entry.id.as_str(), (index, entry))
            .is_some()
        {
            return TranscriptFrame::reset(next, before);
        }
    }
    let mut next_ids = std::collections::HashSet::<&str>::with_capacity(next.len());
    for entry in next {
        if !next_ids.insert(entry.id.as_str()) {
            return TranscriptFrame::reset(next, before);
        }
    }

    let remove: Vec<String> = prev
        .iter()
        .filter(|e| !next_ids.contains(e.id.as_str()))
        .map(|e| e.id.clone())
        .collect();

    let mut upsert = Vec::new();
    let mut append = Vec::new();
    for (i, entry) in next.iter().enumerate() {
        let after = i.checked_sub(1).map(|p| next[p].id.as_str());
        match prev_by_id.get(entry.id.as_str()) {
            Some((prev_ix, prev_entry)) => {
                let prev_after = prev_ix.checked_sub(1).map(|p| prev[p].id.as_str());
                if *prev_entry == entry && prev_after == after {
                    continue;
                }
                if prev_after == after
                    && let Some(text_append) = try_text_append(prev_entry, entry)
                {
                    append.push(text_append);
                    continue;
                }
                upsert.push(TranscriptUpsert {
                    after: after.map(str::to_string),
                    entry: entry.clone(),
                });
            }
            None => upsert.push(TranscriptUpsert {
                after: after.map(str::to_string),
                entry: entry.clone(),
            }),
        }
    }

    // A delta touching most rows serializes like a reset but applies slower.
    if upsert.len() * 2 >= next.len().max(1) && next.len() > 4 {
        return TranscriptFrame::reset(next, before);
    }
    TranscriptFrame::Delta {
        upsert,
        append,
        remove,
        count: next.len(),
        before,
    }
}

/// A frame that could not be applied cleanly — the consumer's copy has
/// diverged (skew, missed frame) and it should resubscribe for a reset.
#[derive(Debug, thiserror::Error)]
#[error("transcript delta desync: {0}")]
pub struct TranscriptDesync(pub String);

/// Apply a frame in place. On any error the state is unreliable — resubscribe.
pub fn apply_transcript_frame(
    current: &mut Vec<SessionMessageEntry>,
    frame: TranscriptFrame,
) -> Result<(), TranscriptDesync> {
    match frame {
        TranscriptFrame::Reset { reset, .. } => {
            *current = reset;
            Ok(())
        }
        TranscriptFrame::Delta {
            upsert,
            append,
            remove,
            count,
            before: _,
        } => {
            let mut current_ids = std::collections::HashSet::<&str>::with_capacity(current.len());
            if let Some(duplicate) = current
                .iter()
                .map(|entry| entry.id.as_str())
                .find(|id| !current_ids.insert(*id))
            {
                return Err(TranscriptDesync(format!(
                    "duplicate current entry id {duplicate}"
                )));
            }
            drop(current_ids);
            let mut operation_ids =
                std::collections::HashSet::<&str>::with_capacity(remove.len() + upsert.len());
            for id in &remove {
                if !operation_ids.insert(id.as_str()) {
                    return Err(TranscriptDesync(format!(
                        "duplicate delta operation for entry {id}"
                    )));
                }
            }
            for TranscriptUpsert { entry, .. } in &upsert {
                if !operation_ids.insert(entry.id.as_str()) {
                    return Err(TranscriptDesync(format!(
                        "duplicate delta operation for entry {}",
                        entry.id
                    )));
                }
            }
            drop(operation_ids);
            if !remove.is_empty() {
                let gone: std::collections::HashSet<&str> =
                    remove.iter().map(String::as_str).collect();
                current.retain(|entry| !gone.contains(entry.id.as_str()));
            }
            for TranscriptUpsert { after, entry } in upsert {
                if let Some(existing) = current
                    .iter()
                    .position(|candidate| candidate.id == entry.id)
                {
                    current.remove(existing);
                }
                let at = match &after {
                    None => 0,
                    Some(anchor) => current
                        .iter()
                        .position(|candidate| &candidate.id == anchor)
                        .map(|index| index + 1)
                        .ok_or_else(|| TranscriptDesync(format!("missing anchor {anchor}")))?,
                };
                current.insert(at, entry);
            }
            for TextAppend {
                entry,
                part,
                text,
                drop_prefix,
                omitted_prefix_bytes,
                len,
            } in append
            {
                let target = current
                    .iter_mut()
                    .find(|candidate| candidate.id == entry)
                    .ok_or_else(|| TranscriptDesync(format!("missing append entry {entry}")))?;
                let target_part = target
                    .parts
                    .iter_mut()
                    .find(|candidate| {
                        matches!(candidate, MessagePart::Text { id, .. } | MessagePart::TextWindow { id, .. } if *id == part)
                    })
                    .ok_or_else(|| TranscriptDesync(format!("missing append part {part}")))?;
                let project_text =
                    matches!(target_part, MessagePart::Text { .. }) && omitted_prefix_bytes > 0;
                {
                    let tail = match target_part {
                        MessagePart::Text { text, .. } => text,
                        MessagePart::TextWindow {
                            text,
                            omitted_prefix_bytes: omitted,
                            ..
                        } => {
                            *omitted = omitted_prefix_bytes;
                            text
                        }
                        _ => unreachable!(),
                    };
                    if drop_prefix > tail.len() || !tail.is_char_boundary(drop_prefix) {
                        return Err(TranscriptDesync(format!(
                            "invalid append prefix drop on {entry}#{part}: {drop_prefix}"
                        )));
                    }
                    tail.drain(..drop_prefix);
                    tail.push_str(&text);
                    if tail.len() != len {
                        return Err(TranscriptDesync(format!(
                            "append length mismatch on {entry}#{part}: have {}, expected {len}",
                            tail.len()
                        )));
                    }
                }
                if project_text && let MessagePart::Text { id, text } = target_part {
                    let id = std::mem::take(id);
                    let text = std::mem::take(text);
                    *target_part = MessagePart::TextWindow {
                        id,
                        text,
                        omitted_prefix_bytes,
                    };
                }
            }
            if current.len() != count {
                return Err(TranscriptDesync(format!(
                    "count mismatch: have {}, expected {count}",
                    current.len()
                )));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parts::MessagePart;
    use crate::schema::MessageRole;

    fn entry(id: &str, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn apply(prev: &[SessionMessageEntry], next: &[SessionMessageEntry]) {
        let frame = diff_transcript(prev, next, None);
        // Round-trip through JSON: the wire shape must survive serde.
        let json = serde_json::to_value(&frame).unwrap();
        let frame: TranscriptFrame = serde_json::from_value(json).unwrap();
        let mut current = prev.to_vec();
        apply_transcript_frame(&mut current, frame).unwrap();
        assert_eq!(&current, next);
    }

    #[test]
    fn append_and_edit_round_trip() {
        let a = entry("a", "hello");
        let b0 = entry("b", "wor");
        let b1 = entry("b", "world");
        apply(&[], std::slice::from_ref(&a));
        apply(std::slice::from_ref(&a), &[a.clone(), b0.clone()]);
        apply(&[a.clone(), b0], &[a, b1]);
    }

    #[test]
    fn remove_and_mid_insert_round_trip() {
        let a = entry("a", "1");
        let b = entry("b", "2");
        let c = entry("c", "3");
        apply(&[a.clone(), b.clone(), c.clone()], &[a.clone(), c.clone()]);
        // Remote merge lands b between a and c.
        apply(&[a.clone(), c.clone()], &[a, b, c]);
    }

    #[test]
    fn streaming_tick_is_a_text_append() {
        let a = entry("a", "prompt");
        let b0 = entry("b", "streaming…");
        let b1 = entry("b", "streaming… more");
        let frame = diff_transcript(&[a.clone(), b0.clone()], &[a.clone(), b1.clone()], None);
        match &frame {
            TranscriptFrame::Delta {
                upsert,
                append,
                remove,
                ..
            } => {
                // The hot path: only the new tokens travel, never the entry.
                assert!(upsert.is_empty());
                assert_eq!(append.len(), 1);
                assert_eq!(append[0].text, " more");
                assert!(remove.is_empty());
            }
            other => panic!("expected delta, got {other:?}"),
        }
        apply(&[a.clone(), b0], &[a, b1]);
    }

    #[test]
    fn sliding_text_window_stays_on_append_deltas() {
        let full = entry("a", "abcdef");
        let mut first_window = full.clone();
        first_window.parts = vec![MessagePart::TextWindow {
            id: "t0".into(),
            text: "cdefg".into(),
            omitted_prefix_bytes: 2,
        }];
        let mut second_window = full.clone();
        second_window.parts = vec![MessagePart::TextWindow {
            id: "t0".into(),
            text: "defgh".into(),
            omitted_prefix_bytes: 3,
        }];

        let first_frame = diff_transcript(
            std::slice::from_ref(&full),
            std::slice::from_ref(&first_window),
            None,
        );
        let TranscriptFrame::Delta { append, upsert, .. } = &first_frame else {
            panic!("expected first sliding delta");
        };
        assert!(upsert.is_empty());
        assert_eq!(append.len(), 1);
        assert_eq!(append[0].drop_prefix, 2);
        assert_eq!(append[0].text, "g");

        let mut current = vec![full];
        apply_transcript_frame(&mut current, first_frame).unwrap();
        assert_eq!(current, vec![first_window.clone()]);

        let second_frame = diff_transcript(
            std::slice::from_ref(&first_window),
            std::slice::from_ref(&second_window),
            None,
        );
        let TranscriptFrame::Delta { append, upsert, .. } = &second_frame else {
            panic!("expected second sliding delta");
        };
        assert!(upsert.is_empty());
        assert_eq!(append.len(), 1);
        assert_eq!(append[0].drop_prefix, 1);
        assert_eq!(append[0].text, "h");

        apply_transcript_frame(&mut current, second_frame).unwrap();
        assert_eq!(current, vec![second_window]);
    }

    #[test]
    fn bounded_tail_retires_oldest_entry_as_a_delta() {
        let prev: Vec<_> = (0..64)
            .map(|index| entry(&format!("m{index}"), "text"))
            .collect();
        let next: Vec<_> = (1..65)
            .map(|index| entry(&format!("m{index}"), "text"))
            .collect();
        let frame = diff_transcript(&prev, &next, Some(1));
        assert_eq!(frame.before(), Some(1));
        assert!(matches!(frame, TranscriptFrame::Delta { .. }));
        let mut current = prev;
        apply_transcript_frame(&mut current, frame).unwrap();
        assert_eq!(current, next);
    }

    #[test]
    fn non_append_change_falls_back_to_upsert() {
        // Same id but a rewritten (non-prefix) text must re-send the entry.
        let b0 = entry("b", "draft text");
        let b1 = entry("b", "final");
        let frame = diff_transcript(std::slice::from_ref(&b0), std::slice::from_ref(&b1), None);
        match &frame {
            TranscriptFrame::Delta { upsert, append, .. } => {
                assert_eq!(upsert.len(), 1);
                assert!(append.is_empty());
            }
            other => panic!("expected delta, got {other:?}"),
        }
        apply(&[b0], &[b1]);
    }

    #[test]
    fn append_length_mismatch_is_desync() {
        let frame = TranscriptFrame::Delta {
            upsert: vec![],
            append: vec![TextAppend {
                entry: "a".into(),
                part: "t0".into(),
                text: "x".into(),
                drop_prefix: 0,
                omitted_prefix_bytes: 0,
                len: 99,
            }],
            remove: vec![],
            count: 1,
            before: None,
        };
        let mut current = vec![entry("a", "hello")];
        assert!(apply_transcript_frame(&mut current, frame).is_err());
    }

    #[test]
    fn large_change_falls_back_to_reset() {
        let prev: Vec<_> = (0..10).map(|i| entry(&format!("p{i}"), "x")).collect();
        let next: Vec<_> = (0..10).map(|i| entry(&format!("n{i}"), "y")).collect();
        assert!(matches!(
            diff_transcript(&prev, &next, None),
            TranscriptFrame::Reset { .. }
        ));
    }

    #[test]
    fn duplicate_ids_force_reset_before_delta_can_reorder() {
        let first = entry("duplicate", "first");
        let middle = entry("middle", "middle");
        let second = entry("duplicate", "second");
        let inserted = entry("inserted", "inserted");
        let prev = vec![first.clone(), middle.clone(), second.clone()];
        let next = vec![first, middle, second, inserted];

        assert!(matches!(
            diff_transcript(&prev, &next, None),
            TranscriptFrame::Reset { .. }
        ));
    }

    #[test]
    fn delta_rejects_a_legacy_transcript_with_duplicate_ids() {
        let frame = TranscriptFrame::Delta {
            upsert: vec![],
            append: vec![],
            remove: vec![],
            count: 2,
            before: None,
        };
        let mut current = vec![entry("duplicate", "first"), entry("duplicate", "second")];

        assert!(apply_transcript_frame(&mut current, frame).is_err());
    }

    #[test]
    fn desync_surfaces_instead_of_corrupting() {
        let frame = TranscriptFrame::Delta {
            upsert: vec![TranscriptUpsert {
                after: Some("missing".into()),
                entry: entry("x", "1"),
            }],
            append: vec![],
            remove: vec![],
            count: 2,
            before: None,
        };
        let mut current = Vec::new();
        assert!(apply_transcript_frame(&mut current, frame).is_err());
    }
}
