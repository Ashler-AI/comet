//! Message parts: the event fold, the render-only privacy policy, and continuation splitting.
//!
//! Ports of `packages/control/src/parts.ts` (fold) and
//! `packages/session-doc/src/{render-parts,messages}.ts`.

use serde::{Deserialize, Serialize};

use comet_proto::{AgentEvent, OMP_GOAL_STATE_CALL_NAME, ToolCall, UserInputQuestion};

use crate::constants::MSG_INLINE_MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageStatus {
    Streaming,
    Complete,
    Aborted,
    /// User followup accepted for delivery after the active turn settles.
    Queued,
    /// User followup sent to the active harness's steering boundary.
    Steered,
}

/// One rendered part of an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MessagePart {
    Text {
        id: String,
        text: String,
    },
    /// Bounded read projection of an oversized stored `Text` part. The text is
    /// the pure visible tail; omitted bytes stay scalar so streaming deltas do
    /// not rewrite a marker embedded in the content.
    TextWindow {
        id: String,
        text: String,
        omitted_prefix_bytes: usize,
    },
    #[serde(rename_all = "camelCase")]
    Tool {
        id: String,
        call: ToolCall,
        #[serde(default)]
        is_error: bool,
        /// True once a ToolResult arrived.
        #[serde(default)]
        resolved: bool,
    },
    #[serde(rename_all = "camelCase")]
    Input {
        id: String,
        request_id: String,
        questions: Vec<UserInputQuestion>,
        #[serde(default)]
        resolved: bool,
    },
    Error {
        id: String,
        message: String,
    },
}

impl MessagePart {
    pub fn id(&self) -> &str {
        match self {
            MessagePart::Text { id, .. }
            | MessagePart::TextWindow { id, .. }
            | MessagePart::Tool { id, .. }
            | MessagePart::Input { id, .. }
            | MessagePart::Error { id, .. } => id,
        }
    }

    pub fn byte_len(&self) -> usize {
        match self {
            MessagePart::Text { text, .. } | MessagePart::TextWindow { text, .. } => text.len(),
            MessagePart::Tool { call, .. } => serde_json::to_vec(call).map_or(0, |v| v.len()),
            MessagePart::Input { questions, .. } => {
                serde_json::to_vec(questions).map_or(0, |v| v.len())
            }
            MessagePart::Error { message, .. } => message.len(),
        }
    }
}

fn intrinsic_call_state(call: &ToolCall) -> Option<(bool, bool)> {
    match call {
        ToolCall::Todo { .. } => Some((true, false)),
        ToolCall::Agent { agents } => {
            let resolved = !agents.is_empty()
                && agents.iter().all(|agent| {
                    matches!(
                        agent.status,
                        comet_proto::AgentActivityStatus::Completed
                            | comet_proto::AgentActivityStatus::Failed
                            | comet_proto::AgentActivityStatus::Cancelled
                    )
                });
            let is_error = agents
                .iter()
                .any(|agent| agent.status == comet_proto::AgentActivityStatus::Failed);
            Some((resolved, is_error))
        }
        _ => None,
    }
}

fn refresh_tool_call(existing: &mut ToolCall, incoming: &ToolCall) {
    if let (
        ToolCall::Todo {
            items: current_items,
        },
        ToolCall::Todo {
            items: incoming_items,
        },
    ) = (&mut *existing, incoming)
    {
        let overlaps = incoming_items.iter().any(|incoming| {
            current_items
                .iter()
                .any(|current| current.text == incoming.text)
        });
        if overlaps {
            for incoming in incoming_items {
                if let Some(current) = current_items
                    .iter_mut()
                    .find(|current| current.text == incoming.text)
                {
                    current.done = incoming.done;
                } else {
                    current_items.push(incoming.clone());
                }
            }
            return;
        }
    }
    *existing = incoming.clone();
}

/// Fold one agent event into a parts accumulator, in place.
///
/// In place because the fold runs once per streamed event: rebuilding the
/// accumulator each time made long turns O(n²) in allocations.
///
/// Semantics from comet `foldEventIntoParts`:
/// - `SessionStarted` / `Steered` reset the accumulator (turn boundary — makes replay safe).
/// - `TextDelta` appends to the trailing text part, or starts a new one if the trail is not text
///   (a tool call in between breaks the text block).
/// - `ToolCall` appends, or refreshes in place when the id already exists (SDK retry idempotence).
/// - `ToolResult` marks the matching tool part resolved / errored in place.
/// - `InputRequested` appends an input part; `InputResolved` marks it resolved.
/// - `Error` and `Done{error}` become visible error parts; a terminal `Done` reusing the latest
///   error message is idempotent.
pub fn fold_event_into_parts(out: &mut Vec<MessagePart>, event: &AgentEvent) {
    match event {
        AgentEvent::SessionStarted { .. } | AgentEvent::Steered { .. } => {
            out.clear();
        }
        AgentEvent::TextDelta { text } => {
            if let Some(MessagePart::Text { text: tail, .. }) = out.last_mut() {
                tail.push_str(text);
            } else {
                let id = format!("t{}", out.len());
                out.push(MessagePart::Text {
                    id,
                    text: text.clone(),
                });
            }
        }
        AgentEvent::ReasoningDelta { .. } => {
            // Reasoning is not rendered as a transcript part (matches comet).
        }
        AgentEvent::ToolCall { id, call } => {
            let intrinsic_state = intrinsic_call_state(call);
            if let Some((existing, is_error, resolved)) = out.iter_mut().find_map(|p| match p {
                MessagePart::Tool {
                    id: pid,
                    call,
                    is_error,
                    resolved,
                } if pid == id => Some((call, is_error, resolved)),
                _ => None,
            }) {
                refresh_tool_call(existing, call);
                if let Some((next_resolved, next_error)) = intrinsic_state {
                    *resolved = next_resolved;
                    *is_error = next_error;
                }
            } else {
                let (resolved, is_error) = intrinsic_state.unwrap_or((false, false));
                out.push(MessagePart::Tool {
                    id: id.clone(),
                    call: call.clone(),
                    is_error,
                    resolved,
                });
            }
        }
        AgentEvent::ToolResult { id, is_error, .. } => {
            for p in out.iter_mut() {
                if let MessagePart::Tool {
                    id: pid,
                    is_error: e,
                    resolved,
                    ..
                } = p
                    && pid == id
                {
                    *e = *is_error;
                    *resolved = true;
                }
            }
        }
        AgentEvent::InputRequested {
            request_id,
            questions,
        } => {
            let id = format!("in-{request_id}");
            if !out.iter().any(|p| p.id() == id) {
                out.push(MessagePart::Input {
                    id,
                    request_id: request_id.clone(),
                    questions: questions.clone(),
                    resolved: false,
                });
            }
        }
        AgentEvent::InputResolved { request_id } => {
            for p in out.iter_mut() {
                if let MessagePart::Input {
                    request_id: rid,
                    resolved,
                    ..
                } = p
                    && rid == request_id
                {
                    *resolved = true;
                }
            }
        }
        AgentEvent::Error { message } => {
            let id = format!("e{}", out.len());
            out.push(MessagePart::Error {
                id,
                message: message.clone(),
            });
        }
        AgentEvent::Done { error, .. } => {
            if let Some(message) = error
                && !matches!(out.last(), Some(MessagePart::Error { message: existing, .. }) if existing == message)
            {
                let id = format!("e{}", out.len());
                out.push(MessagePart::Error {
                    id,
                    message: message.clone(),
                });
            }
        }
        AgentEvent::SessionTitleChanged { .. }
        | AgentEvent::AssistantMessageCompleted { .. }
        | AgentEvent::Usage { .. } => {}
    }
}

/// Render-only privacy policy — strip heavy/sensitive tool inputs before a call enters the doc.
///
/// Keeps: command / path / pattern / url / query / todo items / server+tool names,
/// plus Comet's normalized OMP goal snapshot (the sidebar's persisted state).
/// Drops: WriteFile content, EditFile old/new strings, WebFetch prompt, Mcp/other Unknown input.
/// Full inputs remain only in the host's local run journal. Idempotent.
pub fn sanitize_tool_call(call: &ToolCall) -> ToolCall {
    match call {
        ToolCall::WriteFile { path, .. } => ToolCall::WriteFile {
            path: path.clone(),
            content: None,
        },
        ToolCall::EditFile { path, .. } => ToolCall::EditFile {
            path: path.clone(),
            old_string: None,
            new_string: None,
        },
        ToolCall::WebFetch { url, .. } => ToolCall::WebFetch {
            url: url.clone(),
            prompt: None,
        },
        ToolCall::Mcp { server, tool, .. } => ToolCall::Mcp {
            server: server.clone(),
            tool: tool.clone(),
            input: None,
        },
        ToolCall::Unknown { name, input } if name == OMP_GOAL_STATE_CALL_NAME => {
            ToolCall::Unknown {
                name: name.clone(),
                input: input.clone(),
            }
        }
        ToolCall::Unknown { name, .. } => ToolCall::Unknown {
            name: name.clone(),
            input: None,
        },
        other => other.clone(),
    }
}

/// Deterministic continuation id: `"{root}#c{n}"`.
pub fn continuation_id(root: &str, index: usize) -> String {
    format!("{root}#c{index}")
}

/// Split an oversized parts list into chunks each under `MSG_INLINE_MAX` bytes.
///
/// Splitting happens at part boundaries; an oversized text part is itself chunked at char
/// boundaries. Returns one Vec per resulting entry — the first keeps the root id, the rest are
/// continuations (`continuation_id(root, i)`), matching `splitMessageEntry` in comet.
pub fn split_parts(parts: &[MessagePart]) -> Vec<Vec<MessagePart>> {
    let mut chunks: Vec<Vec<MessagePart>> = vec![Vec::new()];
    let mut current_bytes = 0usize;

    let push_part = |chunks: &mut Vec<Vec<MessagePart>>, current: &mut usize, part: MessagePart| {
        let len = part.byte_len();
        if *current > 0 && *current + len > MSG_INLINE_MAX {
            chunks.push(Vec::new());
            *current = 0;
        }
        *current += len;
        chunks.last_mut().unwrap().push(part);
    };

    for part in parts {
        match part {
            MessagePart::Text { id, text } if text.len() > MSG_INLINE_MAX => {
                // Chunk oversized text at char boundaries.
                let mut start = 0usize;
                let mut piece = 0usize;
                while start < text.len() {
                    let mut end = (start + MSG_INLINE_MAX).min(text.len());
                    while end < text.len() && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    // Guard: ensure forward progress on pathological boundaries.
                    if end <= start {
                        end = text.len();
                    }
                    let sub = MessagePart::Text {
                        id: if piece == 0 {
                            id.clone()
                        } else {
                            format!("{id}~{piece}")
                        },
                        text: text[start..end].to_string(),
                    };
                    push_part(&mut chunks, &mut current_bytes, sub);
                    start = end;
                    piece += 1;
                }
            }
            other => push_part(&mut chunks, &mut current_bytes, other.clone()),
        }
    }
    chunks
}

/// Render-time inverse of splitting: concatenate continuation entries' parts in list order.
pub fn join_continuations(entries: Vec<Vec<MessagePart>>) -> Vec<MessagePart> {
    entries.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_delta(s: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: s.into() }
    }

    #[test]
    fn text_deltas_merge_until_broken_by_tool() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &text_delta("Hello "));
        fold_event_into_parts(&mut parts, &text_delta("world"));
        assert_eq!(parts.len(), 1);
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        fold_event_into_parts(&mut parts, &text_delta("after"));
        assert_eq!(parts.len(), 3);
        match &parts[2] {
            MessagePart::Text { text, .. } => assert_eq!(text, "after"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn session_started_resets_accumulator() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &text_delta("junk"));
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::SessionStarted {
                harness: comet_proto::HarnessId::Mock,
                model: "m".into(),
                tools: vec![],
                cwd: "/".into(),
                session_id: "s".into(),
                assistant_message_id: "a".into(),
            },
        );
        assert!(parts.is_empty());
    }

    #[test]
    fn terminal_done_does_not_duplicate_the_immediately_preceding_error() {
        let message = "The socket connection was closed unexpectedly";
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::Error {
                message: message.into(),
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::Done {
                status: comet_proto::DoneStatus::Errored,
                result: None,
                error: Some(message.into()),
                session_id: Some("omp-session".into()),
            },
        );

        assert_eq!(parts.len(), 1);
        assert!(matches!(
            &parts[0],
            MessagePart::Error { message: visible, .. } if visible == message
        ));
    }

    #[test]
    fn terminal_done_preserves_a_distinct_error() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::Error {
                message: "provider warning".into(),
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::Done {
                status: comet_proto::DoneStatus::Errored,
                result: None,
                error: Some("terminal failure".into()),
                session_id: None,
            },
        );

        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn tool_call_refresh_is_idempotent() {
        let call = AgentEvent::ToolCall {
            id: "t".into(),
            call: ToolCall::Exec {
                command: "ls".into(),
            },
        };
        let mut once = Vec::new();
        fold_event_into_parts(&mut once, &call);
        let mut twice = once.clone();
        fold_event_into_parts(&mut twice, &call);
        assert_eq!(once, twice);
    }

    #[test]
    fn tool_result_marks_resolution() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "t".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolResult {
                id: "t".into(),
                is_error: true,
                output: None,
            },
        );
        match &parts[0] {
            MessagePart::Tool {
                is_error, resolved, ..
            } => {
                assert!(*is_error);
                assert!(*resolved);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn agent_activity_updates_in_place_and_resolves_from_terminal_statuses() {
        use comet_proto::{AgentActivity, AgentActivityStatus};

        let event = |status| AgentEvent::ToolCall {
            id: "task-1".into(),
            call: ToolCall::Agent {
                agents: vec![AgentActivity {
                    id: "Scout".into(),
                    role: "scout".into(),
                    status,
                    model: Some("anthropic/claude-opus-5:xhigh".into()),
                }],
            },
        };
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &event(AgentActivityStatus::Pending));
        fold_event_into_parts(&mut parts, &event(AgentActivityStatus::Running));
        assert_eq!(parts.len(), 1);
        assert!(matches!(
            &parts[0],
            MessagePart::Tool {
                resolved: false,
                is_error: false,
                ..
            }
        ));

        fold_event_into_parts(&mut parts, &event(AgentActivityStatus::Completed));
        assert!(matches!(
            &parts[0],
            MessagePart::Tool {
                resolved: true,
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn todo_snapshots_are_complete_when_persisted() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "omp-plan".into(),
                call: ToolCall::Todo {
                    items: vec![comet_proto::TodoItem {
                        text: "Ship it".into(),
                        done: false,
                    }],
                },
            },
        );

        assert!(matches!(
            &parts[0],
            MessagePart::Tool {
                resolved: true,
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn todo_refresh_preserves_completed_items_omitted_by_later_updates() {
        let event = |items| AgentEvent::ToolCall {
            id: "omp-plan".into(),
            call: ToolCall::Todo { items },
        };
        let item = |text: &str, done| comet_proto::TodoItem {
            text: text.into(),
            done,
        };
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &event(vec![item("Goal alpha", false), item("Goal beta", false)]),
        );
        fold_event_into_parts(
            &mut parts,
            &event(vec![item("Goal alpha", true), item("Goal beta", false)]),
        );
        fold_event_into_parts(&mut parts, &event(vec![item("Goal beta", false)]));

        assert!(matches!(
            &parts[0],
            MessagePart::Tool {
                call: ToolCall::Todo { items },
                ..
            } if items == &vec![item("Goal alpha", true), item("Goal beta", false)]
        ));
    }

    #[test]
    fn todo_refresh_replaces_an_unrelated_plan() {
        let mut parts = Vec::new();
        for (text, done) in [("Old goal", true), ("New goal", false)] {
            fold_event_into_parts(
                &mut parts,
                &AgentEvent::ToolCall {
                    id: "omp-plan".into(),
                    call: ToolCall::Todo {
                        items: vec![comet_proto::TodoItem {
                            text: text.into(),
                            done,
                        }],
                    },
                },
            );
        }

        assert!(matches!(
            &parts[0],
            MessagePart::Tool {
                call: ToolCall::Todo { items },
                ..
            } if items.len() == 1 && items[0].text == "New goal" && !items[0].done
        ));
    }

    #[test]
    fn sanitize_strips_heavy_inputs_and_is_idempotent() {
        let call = ToolCall::WriteFile {
            path: "/x".into(),
            content: Some("secret".into()),
        };
        let clean = sanitize_tool_call(&call);
        assert_eq!(
            clean,
            ToolCall::WriteFile {
                path: "/x".into(),
                content: None
            }
        );
        assert_eq!(sanitize_tool_call(&clean), clean);
    }

    #[test]
    fn sanitize_preserves_normalized_omp_goal_state() {
        let call = ToolCall::Unknown {
            name: OMP_GOAL_STATE_CALL_NAME.into(),
            input: Some(serde_json::json!({
                "goal": { "id": "g1", "objective": "Ship the sidebar", "status": "active" },
                "state": { "enabled": true, "mode": "active" },
            })),
        };
        assert_eq!(sanitize_tool_call(&call), call);
    }

    #[test]
    fn split_and_join_round_trip() {
        let big = "x".repeat(MSG_INLINE_MAX * 2 + 100);
        let parts = vec![
            MessagePart::Text {
                id: "t0".into(),
                text: big.clone(),
            },
            MessagePart::Tool {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
                is_error: false,
                resolved: true,
            },
        ];
        let chunks = split_parts(&parts);
        assert!(
            chunks.len() >= 3,
            "expected >=3 chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            let bytes: usize = chunk.iter().map(|p| p.byte_len()).sum();
            assert!(bytes <= MSG_INLINE_MAX, "chunk over cap: {bytes}");
        }
        let joined = join_continuations(chunks);
        let text: String = joined
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, big);
        assert!(matches!(joined.last().unwrap(), MessagePart::Tool { .. }));
    }

    #[test]
    fn continuation_ids_are_deterministic() {
        assert_eq!(continuation_id("m1", 1), "m1#c1");
    }
}
