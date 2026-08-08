//! Durable command ledger — port of `packages/session-doc/src/commands.ts`.
//!
//! Rules:
//! 1. Each device inserts only its own entries; entries are append-only and immutable.
//! 2. The addressed agent-session owner executes and writes the outcome; a composer may
//!    only cancel its own still-pending entry. Shared threads can have several owners.
//! 3. Evaluation (`evaluate_command`, pure): processed-id dedupe → Skip; expired TTL → Expired;
//!    a newer command of the same kind supersedes steer/interrupt; an interrupt whose
//!    `based_on.turn_id` is already past → Superseded; otherwise Execute.

use serde::{Deserialize, Serialize};

use comet_proto::{
    AgentSessionSource, CapabilityGrant, CollaborationPrincipal, RunRequest, SemanticAnchor,
    SemanticAnnotation, UserInputAnswer,
};

use crate::constants::COMMAND_DEFAULT_TTL_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionCommandKind {
    Run,
    Steer,
    Queue,
    Interrupt,
    RespondInput,
    PeerMessage,
    Control,
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
#[serde(tag = "action", rename_all = "camelCase")]
pub enum SessionControlAction {
    Start {
        request: RunRequest,
        message_id: String,
    },
    Steer {
        prompt: String,
        message_id: Option<String>,
    },
    Queue {
        prompt: String,
        message_id: Option<String>,
    },
    RespondInput {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    Pause {},
    Resume {},
    Stop {},
    Focus {
        target_id: String,
    },
    AnnotationCreate {
        annotation: SemanticAnnotation,
    },
    AnnotationEdit {
        annotation_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<SemanticAnchor>,
    },
    AnnotationResolve {
        annotation_id: String,
        resolved: bool,
    },
    EnvironmentLifecycle {
        operation: String,
        #[serde(default)]
        parameters: serde_json::Map<String, serde_json::Value>,
    },
}

impl SessionControlAction {
    pub fn required_capability(&self) -> &'static str {
        match self {
            Self::Start { .. }
            | Self::Steer { .. }
            | Self::Queue { .. }
            | Self::RespondInput { .. } => comet_proto::CAPABILITY_SESSION_CHAT,
            Self::AnnotationCreate { .. }
            | Self::AnnotationEdit { .. }
            | Self::AnnotationResolve { .. } => comet_proto::CAPABILITY_SESSION_ANNOTATE,
            Self::EnvironmentLifecycle { .. } => comet_proto::CAPABILITY_SESSION_ENVIRONMENT,
            Self::Pause {} | Self::Resume {} | Self::Stop {} | Self::Focus { .. } => {
                comet_proto::CAPABILITY_SESSION_CONTROL
            }
        }
    }
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
    #[serde(rename_all = "camelCase")]
    Queue {
        prompt: String,
        message_id: Option<String>,
    },
    Interrupt {},
    #[serde(rename_all = "camelCase")]
    RespondInput {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    #[serde(rename_all = "camelCase")]
    PeerMessage {
        text: String,
        source_chat_id: String,
        thread_id: String,
        reply_to: Option<String>,
        hop_count: u8,
    },
    #[serde(rename_all = "camelCase")]
    Control {
        session_id: String,
        owner_device_id: String,
        actor_device_id: String,
        actor_subject: String,
        grant_id: String,
        source: AgentSessionSource,
        action: Box<SessionControlAction>,
    },
}

impl SessionCommandPayload {
    pub fn kind(&self) -> SessionCommandKind {
        match self {
            SessionCommandPayload::Run { .. } => SessionCommandKind::Run,
            SessionCommandPayload::Steer { .. } => SessionCommandKind::Steer,
            SessionCommandPayload::Queue { .. } => SessionCommandKind::Queue,
            SessionCommandPayload::Interrupt {} => SessionCommandKind::Interrupt,
            SessionCommandPayload::RespondInput { .. } => SessionCommandKind::RespondInput,
            SessionCommandPayload::PeerMessage { .. } => SessionCommandKind::PeerMessage,
            SessionCommandPayload::Control { .. } => SessionCommandKind::Control,
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
    #[serde(default)]
    pub resolution: Option<String>,
}

impl SessionCommandEntry {
    pub fn kind(&self) -> SessionCommandKind {
        self.payload.kind()
    }

    pub fn effective_expiry(&self) -> i64 {
        self.expires_at
            .unwrap_or(self.issued_at + COMMAND_DEFAULT_TTL_MS)
    }

    pub fn session_owner(&self) -> Option<(&str, &str)> {
        match &self.payload {
            SessionCommandPayload::Control {
                session_id,
                owner_device_id,
                ..
            } => Some((session_id, owner_device_id)),
            _ => None,
        }
    }

    pub fn actor_subject(&self) -> &str {
        match &self.payload {
            SessionCommandPayload::Control { actor_subject, .. } => actor_subject,
            _ => &self.issued_by,
        }
    }

    pub fn action_name(&self) -> &'static str {
        match &self.payload {
            SessionCommandPayload::Run { .. } => "start",
            SessionCommandPayload::Steer { .. } => "steer",
            SessionCommandPayload::Queue { .. } => "queue",
            SessionCommandPayload::Interrupt {} => "stop",
            SessionCommandPayload::RespondInput { .. } => "respondInput",
            SessionCommandPayload::PeerMessage { .. } => "peerMessage",
            SessionCommandPayload::Control { action, .. } => match action.as_ref() {
                SessionControlAction::Start { .. } => "start",
                SessionControlAction::Steer { .. } => "steer",
                SessionControlAction::Queue { .. } => "queue",
                SessionControlAction::RespondInput { .. } => "respondInput",
                SessionControlAction::Pause {} => "pause",
                SessionControlAction::Resume {} => "resume",
                SessionControlAction::Stop {} => "stop",
                SessionControlAction::Focus { .. } => "focus",
                SessionControlAction::AnnotationCreate { .. } => "annotationCreate",
                SessionControlAction::AnnotationEdit { .. } => "annotationEdit",
                SessionControlAction::AnnotationResolve { .. } => "annotationResolve",
                SessionControlAction::EnvironmentLifecycle { .. } => "environmentLifecycle",
            },
        }
    }

    pub fn required_capability(&self) -> Option<&'static str> {
        match &self.payload {
            SessionCommandPayload::Control { action, .. } => Some(action.required_capability()),
            _ => None,
        }
    }
}

/// Capability check for teammate control. `principal` must come from verified IAP/control-plane
/// claims; command fields and caller-supplied project/device data are never authority.
pub fn authorize_control_command(
    entry: &SessionCommandEntry,
    principal: &CollaborationPrincipal,
    grants: &[CapabilityGrant],
    now_ms: i64,
) -> bool {
    let SessionCommandPayload::Control {
        actor_subject,
        grant_id,
        action,
        ..
    } = &entry.payload
    else {
        return true;
    };
    actor_subject == &principal.subject
        && grants.iter().any(|grant| {
            grant.id == *grant_id && grant.permits(principal, action.required_capability(), now_ms)
        })
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
    // A newer pending command of the same kind supersedes steer/interrupt.
    let kind = entry.kind();
    if matches!(
        kind,
        SessionCommandKind::Steer | SessionCommandKind::Interrupt
    ) {
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
    fn newer_steer_supersedes_older_pending_steer() {
        let older = steer("c1", 1_000);
        let newer = steer("c2", 2_000);
        let entries = vec![older.clone(), newer.clone()];
        let cx1 = cx(&entries, &NEVER, &NEVER, 3_000, None);
        assert_eq!(
            evaluate_command(&older, &cx1),
            CommandDisposition::Superseded
        );
        assert_eq!(evaluate_command(&newer, &cx1), CommandDisposition::Execute);
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
        // Two queued runs both execute (in order); supersession applies to steer/interrupt only.
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
    fn peer_messages_serialize_as_camel_case_and_execute_independently() {
        let payload = SessionCommandPayload::PeerMessage {
            text: "review this".into(),
            source_chat_id: "source".into(),
            thread_id: "thread".into(),
            reply_to: Some("previous".into()),
            hop_count: 3,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["kind"], "peerMessage");
        assert_eq!(json["sourceChatId"], "source");
        assert_eq!(json["threadId"], "thread");
        assert_eq!(json["replyTo"], "previous");
        assert_eq!(json["hopCount"], 3);
        assert_eq!(payload.kind(), SessionCommandKind::PeerMessage);

        let older = entry("p1", payload.clone(), 1_000);
        let newer = entry("p2", payload, 2_000);
        let entries = vec![older.clone(), newer.clone()];
        let context = cx(&entries, &NEVER, &NEVER, 3_000, None);
        assert_eq!(
            evaluate_command(&older, &context),
            CommandDisposition::Execute
        );
        assert_eq!(
            evaluate_command(&newer, &context),
            CommandDisposition::Execute
        );
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
            prompt: "hello".into(),
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "/tmp".into(),
            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
            auto_approve: false,
            attachments: Vec::new(),
            resume: None,
        }
    }
}
