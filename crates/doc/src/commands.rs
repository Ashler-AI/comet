//! Durable command ledger — port of `packages/session-doc/src/commands.ts`.
//!
//! Rules (verbatim from zeron's design):
//! 1. Each device inserts only its own entries; entries are append-only and immutable.
//! 2. The chat's HOST is the sole writer of command outcomes; a composer may only set
//!    `cancelled` on its own still-pending entries.
//! 3. Evaluation (`evaluate_command`, pure): processed-id dedupe → Skip; expired TTL → Expired;
//!    a newer interrupt supersedes an older interrupt; user prompts never supersede each other; an interrupt whose
//!    `based_on.turn_id` is already past → Superseded; otherwise Execute.

use serde::{Deserialize, Serialize};

use zeron_proto::{RunRequest, UserInputAnswer};

use crate::constants::COMMAND_DEFAULT_TTL_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionCommandKind {
    Run,
    Steer,
    Interrupt,
    RespondInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionCommandStatus {
    Pending,
    Applied,
    Rejected,
    Expired,
    Superseded,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SessionCommandPayload {
    #[serde(rename_all = "camelCase")]
    Run {
        request: RunRequest,
        /// Client-minted message id for the optimistic user entry (dedup key).
        message_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Steer {
        prompt: String,
        message_id: Option<String>,
    },
    Interrupt {},
    #[serde(rename_all = "camelCase")]
    RespondInput {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
}

impl SessionCommandPayload {
    pub fn kind(&self) -> SessionCommandKind {
        match self {
            SessionCommandPayload::Run { .. } => SessionCommandKind::Run,
            SessionCommandPayload::Steer { .. } => SessionCommandKind::Steer,
            SessionCommandPayload::Interrupt {} => SessionCommandKind::Interrupt,
            SessionCommandPayload::RespondInput { .. } => SessionCommandKind::RespondInput,
        }
    }

    pub(crate) fn message_id(&self) -> Option<&str> {
        match self {
            Self::Run { message_id, .. } => Some(message_id),
            Self::Steer { message_id, .. } => message_id.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandBasedOn {
    pub turn_id: Option<String>,
    pub frontier: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCommandEntry {
    pub id: String,
    pub payload: SessionCommandPayload,
    pub issued_by: String,
    /// Epoch millis.
    pub issued_at: i64,
    #[serde(default)]
    pub based_on: Option<CommandBasedOn>,
    /// Epoch millis; defaults to issued_at + COMMAND_DEFAULT_TTL_MS when absent.
    #[serde(default)]
    pub expires_at: Option<i64>,
    pub status: SessionCommandStatus,
    /// Epoch millis when the host accepted this attempt for preparation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<i64>,
    #[serde(default)]
    pub resolution: Option<String>,
}

impl SessionCommandEntry {
    pub fn kind(&self) -> SessionCommandKind {
        self.payload.kind()
    }

    pub fn is_accepted(&self) -> bool {
        self.status == SessionCommandStatus::Pending && self.accepted_at.is_some()
    }

    pub fn effective_expiry(&self) -> i64 {
        self.expires_at
            .unwrap_or(self.issued_at + COMMAND_DEFAULT_TTL_MS)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandDeliveryStatus {
    Preparing,
    Failed { reason: String },
}

/// Latest attempt per message wins, including attempts with no delivery overlay.
pub fn command_delivery(
    commands: &[SessionCommandEntry],
) -> std::collections::HashMap<String, CommandDeliveryStatus> {
    project_command_delivery(commands.iter().filter_map(|command| {
        Some((command.payload.message_id()?, command.issued_at, command.status,
            command.accepted_at, command.resolution.as_deref()))
    }))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandReceipt {
    pub message_id: Option<String>,
    pub issued_at: i64,
    pub accepted_at: i64,
    pub status: SessionCommandStatus,
    pub resolution: Option<String>,
}

pub(crate) fn project_command_delivery<'a>(
    attempts: impl Iterator<Item = (&'a str, i64, SessionCommandStatus, Option<i64>, Option<&'a str>)>,
) -> std::collections::HashMap<String, CommandDeliveryStatus> {
    let mut latest = std::collections::HashMap::new();
    for (message_id, issued_at, status, accepted_at, resolution) in attempts {
        let attempt = latest.entry(message_id).or_insert((issued_at, status, accepted_at, resolution));
        if issued_at >= attempt.0 {
            *attempt = (issued_at, status, accepted_at, resolution);
        }
    }
    latest
        .into_iter()
        .filter_map(|(message_id, (_, status, accepted_at, resolution))| {
            let status = match status {
                SessionCommandStatus::Pending if accepted_at.is_some() => {
                    CommandDeliveryStatus::Preparing
                }
                SessionCommandStatus::Rejected | SessionCommandStatus::Expired => {
                    let fallback = if status == SessionCommandStatus::Expired {
                        "Delivery expired. Retry when the host is available."
                    } else {
                        "The host could not prepare this message. Retry sending it."
                    };
                    CommandDeliveryStatus::Failed {
                        reason: resolution
                            .filter(|reason| !reason.trim().is_empty())
                            .unwrap_or(fallback).to_owned(),
                    }
                }
                _ => return None,
            };
            Some((message_id.to_owned(), status))
        })
        .collect()
}

/// Rule 2: only the composer that issued a still-pending command may cancel it.
pub fn can_composer_cancel(entry: &SessionCommandEntry, device_id: &str) -> bool {
    entry.status == SessionCommandStatus::Pending && entry.issued_by == device_id
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandDisposition {
    /// Already in the processed ledger — do nothing (idempotence).
    Skip,
    /// Mark expired.
    Expired,
    /// Mark superseded.
    Superseded,
    /// Mark processed BEFORE executing, then execute.
    Execute,
}

/// Context the host evaluates a pending command against.
pub struct EvaluationContext<'a> {
    /// Processed-command ledger membership test.
    pub is_processed: &'a dyn Fn(&str) -> bool,
    /// Current wall clock, epoch millis.
    pub now_ms: i64,
    /// All command entries in doc order (used to find newer same-kind entries).
    pub entries: &'a [SessionCommandEntry],
    /// The id of the turn currently (or most recently) running, if any.
    pub current_turn_id: Option<&'a str>,
    /// True when the given turn id has already completed.
    pub turn_is_past: &'a dyn Fn(&str) -> bool,
}

/// Rule 3 — pure evaluation of a single pending command.
pub fn evaluate_command(
    entry: &SessionCommandEntry,
    cx: &EvaluationContext<'_>,
) -> CommandDisposition {
    if (cx.is_processed)(&entry.id) {
        return CommandDisposition::Skip;
    }
    if cx.now_ms >= entry.effective_expiry() {
        return CommandDisposition::Expired;
    }
    // Interrupts are redundant control signals; steers are distinct user messages.
    // Coalescing steers loses prompts when remote sync delivers a batch.
    let kind = entry.kind();
    if kind == SessionCommandKind::Interrupt {
        let has_newer_same_kind = cx.entries.iter().any(|other| {
            other.id != entry.id
                && other.kind() == kind
                && other.status == SessionCommandStatus::Pending
                && other.issued_at > entry.issued_at
        });
        if has_newer_same_kind {
            return CommandDisposition::Superseded;
        }
    }
    // An interrupt aimed at a turn that already finished is moot.
    if kind == SessionCommandKind::Interrupt
        && let Some(based_on) = &entry.based_on
        && let Some(turn_id) = &based_on.turn_id
    {
        let is_current = cx.current_turn_id == Some(turn_id.as_str());
        if !is_current && (cx.turn_is_past)(turn_id) {
            return CommandDisposition::Superseded;
        }
    }
    CommandDisposition::Execute
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, payload: SessionCommandPayload, issued_at: i64) -> SessionCommandEntry {
        SessionCommandEntry {
            id: id.into(),
            payload,
            issued_by: "device-a".into(),
            issued_at,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            accepted_at: None,
            resolution: None,
        }
    }

    fn steer(id: &str, issued_at: i64) -> SessionCommandEntry {
        entry(
            id,
            SessionCommandPayload::Steer {
                prompt: "go".into(),
                message_id: None,
            },
            issued_at,
        )
    }

    #[test]
    fn delivery_projection_uses_latest_attempt_and_typed_acceptance() {
        let mut failed = entry("old", SessionCommandPayload::Steer {
            prompt: "go".into(), message_id: Some("message".into()),
        }, 1);
        failed.status = SessionCommandStatus::Rejected;
        failed.resolution = Some("Select an available model and retry.".into());
        assert_eq!(command_delivery(&[failed.clone()])["message"],
            CommandDeliveryStatus::Failed { reason: failed.resolution.clone().unwrap() });
        let mut retry = failed.clone();
        retry.id = "retry".into();
        retry.issued_at = 2;
        retry.status = SessionCommandStatus::Pending;
        assert!(!retry.is_accepted());
        assert!(command_delivery(&[retry.clone(), failed.clone()]).is_empty());
        retry.accepted_at = Some(3);
        assert!(retry.is_accepted());
        assert_eq!(command_delivery(&[failed.clone(), retry.clone()])["message"],
            CommandDeliveryStatus::Preparing);
        retry.status = SessionCommandStatus::Applied;
        assert!(!retry.is_accepted());
        assert!(command_delivery(&[failed, retry.clone()]).is_empty());
        retry.status = SessionCommandStatus::Expired;
        retry.resolution = None;
        assert!(matches!(&command_delivery(&[retry])["message"],
            CommandDeliveryStatus::Failed { reason } if reason.contains("Retry")));
        let legacy = serde_json::to_value(steer("legacy", 1)).unwrap();
        assert!(legacy.get("acceptedAt").is_none());
        assert!(!serde_json::from_value::<SessionCommandEntry>(legacy).unwrap().is_accepted());
    }

    fn cx<'a>(
        entries: &'a [SessionCommandEntry],
        processed: &'a dyn Fn(&str) -> bool,
        turn_is_past: &'a dyn Fn(&str) -> bool,
        now_ms: i64,
        current_turn_id: Option<&'a str>,
    ) -> EvaluationContext<'a> {
        EvaluationContext {
            is_processed: processed,
            now_ms,
            entries,
            current_turn_id,
            turn_is_past,
        }
    }

    const NEVER: fn(&str) -> bool = |_| false;

    #[test]
    fn processed_commands_are_skipped() {
        let e = steer("c1", 1_000);
        let entries = vec![e.clone()];
        let processed = |id: &str| id == "c1";
        let cx = cx(&entries, &processed, &NEVER, 2_000, None);
        assert_eq!(evaluate_command(&e, &cx), CommandDisposition::Skip);
    }

    #[test]
    fn expired_commands_are_expired() {
        let e = steer("c1", 0);
        let entries = vec![e.clone()];
        let cx = cx(&entries, &NEVER, &NEVER, COMMAND_DEFAULT_TTL_MS + 1, None);
        assert_eq!(evaluate_command(&e, &cx), CommandDisposition::Expired);
    }

    #[test]
    fn newer_steer_preserves_older_pending_steer() {
        let older = steer("c1", 1_000);
        let newer = steer("c2", 2_000);
        let entries = vec![older.clone(), newer.clone()];
        let cx1 = cx(&entries, &NEVER, &NEVER, 3_000, None);
        assert_eq!(evaluate_command(&older, &cx1), CommandDisposition::Execute);
        assert_eq!(evaluate_command(&newer, &cx1), CommandDisposition::Execute);
    }

    #[test]
    fn batched_remote_steers_all_execute_once_even_with_clock_skew() {
        use std::collections::HashSet;
        for count in [2, 20, 200] {
            let entries: Vec<_> = (0..count)
                .map(|i| {
                    entry(
                        &format!("c{i}"),
                        SessionCommandPayload::Steer {
                            prompt: format!("unique message {i}"),
                            message_id: Some(format!("m{i}")),
                        },
                        1_000 + (i * 17 % 23),
                    )
                })
                .collect();
            let mut processed = HashSet::new();
            for _ in 0..3 {
                for entry in &entries {
                    let expected = if processed.contains(&entry.id) {
                        CommandDisposition::Skip
                    } else {
                        CommandDisposition::Execute
                    };
                    let seen = |id: &str| processed.contains(id);
                    let context = cx(&entries, &seen, &NEVER, 2_000, None);
                    assert_eq!(evaluate_command(entry, &context), expected);
                    processed.insert(entry.id.clone());
                }
            }
            assert_eq!(processed.len(), count as usize);
        }
    }

    #[test]
    fn newer_interrupt_still_supersedes_older_interrupt() {
        let entries = vec![
            entry("i1", SessionCommandPayload::Interrupt {}, 1_000),
            entry("i2", SessionCommandPayload::Interrupt {}, 2_000),
        ];
        let context = cx(&entries, &NEVER, &NEVER, 3_000, None);
        assert_eq!(
            evaluate_command(&entries[0], &context),
            CommandDisposition::Superseded
        );
        assert_eq!(
            evaluate_command(&entries[1], &context),
            CommandDisposition::Execute
        );
    }

    #[test]
    fn interrupt_for_past_turn_is_superseded() {
        let mut e = entry("c1", SessionCommandPayload::Interrupt {}, 1_000);
        e.based_on = Some(CommandBasedOn {
            turn_id: Some("turn-1".into()),
            frontier: None,
        });
        let entries = vec![e.clone()];
        let past = |id: &str| id == "turn-1";
        let cx1 = cx(&entries, &NEVER, &past, 2_000, Some("turn-2"));
        assert_eq!(evaluate_command(&e, &cx1), CommandDisposition::Superseded);
        // …but if that turn is still the current one, execute.
        let cx2 = cx(&entries, &NEVER, &past, 2_000, Some("turn-1"));
        assert_eq!(evaluate_command(&e, &cx2), CommandDisposition::Execute);
    }

    #[test]
    fn runs_are_not_superseded_by_newer_runs() {
        // Two queued runs both execute (in order); supersession applies to interrupts only.
        let r1 = entry(
            "r1",
            SessionCommandPayload::Run {
                request: run_request(),
                message_id: "m1".into(),
            },
            1_000,
        );
        let r2 = entry(
            "r2",
            SessionCommandPayload::Run {
                request: run_request(),
                message_id: "m2".into(),
            },
            2_000,
        );
        let entries = vec![r1.clone(), r2.clone()];
        let cx1 = cx(&entries, &NEVER, &NEVER, 3_000, None);
        assert_eq!(evaluate_command(&r1, &cx1), CommandDisposition::Execute);
        assert_eq!(evaluate_command(&r2, &cx1), CommandDisposition::Execute);
    }

    #[test]
    fn composer_cancel_rules() {
        let e = steer("c1", 1_000);
        assert!(can_composer_cancel(&e, "device-a"));
        assert!(!can_composer_cancel(&e, "device-b"));
        let mut applied = e.clone();
        applied.status = SessionCommandStatus::Applied;
        assert!(!can_composer_cancel(&applied, "device-a"));
    }

    fn run_request() -> RunRequest {
        RunRequest {
            mcp: None,
            prompt: "hello".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "/tmp".into(),
            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
            auto_approve: false,
            attachments: Vec::new(),
            worktree: None,
            resume: None,
            env: Default::default(),
        }
    }
}
