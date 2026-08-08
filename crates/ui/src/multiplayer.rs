//! Pure multiplayer projections shared by the GPUI sidebar, toolbar, transcript,
//! and activity drawer.

use comet_proto::{
    AgentSessionSource, AuditResult, CollaborationSnapshot, ParticipantState, PublicationValue,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityTone {
    Neutral,
    Pending,
    Success,
    Danger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityItem {
    pub id: String,
    pub actor: String,
    pub label: String,
    pub detail: Option<String>,
    pub target_id: Option<String>,
    pub occurred_at: i64,
    pub tone: ActivityTone,
}

pub fn initials(name: &str) -> String {
    let mut words = name
        .split(|ch: char| ch.is_whitespace() || ch == '@' || ch == '.' || ch == '_' || ch == '-')
        .filter(|word| !word.is_empty());
    let first = words.next().and_then(|word| word.chars().next());
    let second = words.next().and_then(|word| word.chars().next());
    match (first, second) {
        (Some(first), Some(second)) => format!("{first}{second}").to_uppercase(),
        (Some(first), None) => first.to_uppercase().to_string(),
        _ => "?".into(),
    }
}

pub const fn source_label(source: AgentSessionSource) -> &'static str {
    match source {
        AgentSessionSource::Local => "Local",
        AgentSessionSource::Scaffold => "Scaffold",
    }
}

pub const fn harness_label(harness: comet_proto::HarnessId) -> &'static str {
    match harness {
        comet_proto::HarnessId::ClaudeCode => "Claude Code",
        comet_proto::HarnessId::Codex => "Codex",
        comet_proto::HarnessId::Omp => "OMP",
        comet_proto::HarnessId::PrimeAgent => "Prime Agent",
        comet_proto::HarnessId::OpenCode => "OpenCode",
        comet_proto::HarnessId::Cursor => "Cursor",
        comet_proto::HarnessId::Mock => "Mock",
    }
}

pub const fn presence_label(state: ParticipantState) -> &'static str {
    match state {
        ParticipantState::Active => "Active",
        ParticipantState::Idle => "Idle",
        ParticipantState::Disconnected => "Offline",
    }
}

pub fn runtime_model(runtime: Option<&str>, model: Option<&str>) -> String {
    match (
        runtime.filter(|value| !value.is_empty()),
        model.filter(|value| !value.is_empty()),
    ) {
        (Some(runtime), Some(model)) => format!("{runtime} · {model}"),
        (Some(runtime), None) => runtime.to_string(),
        (None, Some(model)) => model.to_string(),
        (None, None) => "Agent not started".into(),
    }
}

pub fn cursor_location(cursor: &comet_proto::ParticipantCursor) -> String {
    match &cursor.selection {
        Some(range) if range.start != range.end => format!("{}–{}", range.start, range.end),
        _ => cursor.caret.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTargetSource {
    Local,
    AshlerArtifact,
    ScaffoldArtifact,
    Unknown,
}

pub fn file_target_source(
    reference: Option<&comet_proto::FileTargetReference>,
) -> FileTargetSource {
    match reference {
        Some(comet_proto::FileTargetReference::LocalWorkspacePath { .. }) => {
            FileTargetSource::Local
        }
        Some(comet_proto::FileTargetReference::ScaffoldArtifact {
            artifact_uri: Some(uri),
            ..
        }) if uri.starts_with("ashler://artifact/") => FileTargetSource::AshlerArtifact,
        Some(comet_proto::FileTargetReference::ScaffoldArtifact { .. }) => {
            FileTargetSource::ScaffoldArtifact
        }
        None => FileTargetSource::Unknown,
    }
}

pub fn file_target_label(reference: Option<&comet_proto::FileTargetReference>) -> String {
    match reference {
        Some(comet_proto::FileTargetReference::LocalWorkspacePath { relative_path, .. }) => {
            relative_path.clone()
        }
        Some(comet_proto::FileTargetReference::ScaffoldArtifact {
            artifact_id,
            artifact_uri,
            ..
        }) => artifact_uri.clone().unwrap_or_else(|| artifact_id.clone()),
        None => "Unknown file".into(),
    }
}

pub const fn annotation_state_label(state: comet_proto::AnnotationState) -> &'static str {
    match state {
        comet_proto::AnnotationState::Anchored => "Anchored",
        comet_proto::AnnotationState::Reanchored => "Re-anchored",
        comet_proto::AnnotationState::Orphaned => "Orphaned",
    }
}

pub const fn file_target_source_label(source: FileTargetSource) -> &'static str {
    match source {
        FileTargetSource::Local => "Local file",
        FileTargetSource::AshlerArtifact => "Ashler artifact",
        FileTargetSource::ScaffoldArtifact => "Scaffold artifact",
        FileTargetSource::Unknown => "File",
    }
}

pub fn bounded_anchor_exact(text: &str) -> String {
    const LIMIT: usize = 4096;
    let mut end = text.len().min(LIMIT);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

pub fn quoted_anchor(
    target_kind: comet_proto::AnchorTargetKind,
    target_id: String,
    text: &str,
) -> comet_proto::SemanticAnchor {
    const EXACT_LIMIT: usize = 256;
    const CONTEXT_LIMIT: usize = 64;
    let mut end = text.len().min(EXACT_LIMIT);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut suffix_end = (end + CONTEXT_LIMIT).min(text.len());
    while suffix_end > end && !text.is_char_boundary(suffix_end) {
        suffix_end -= 1;
    }
    comet_proto::SemanticAnchor {
        target_kind,
        target_id,
        file: None,
        byte_range: Some(comet_proto::Utf8ByteRange {
            start: 0,
            end: end as u32,
        }),
        exact: Some(text[..end].to_string()),
        prefix_hash: Some(comet_doc::anchor_context_hash("")),
        suffix_hash: Some(comet_doc::anchor_context_hash(&text[end..suffix_end])),
        unknown: Default::default(),
    }
}

pub fn local_file_anchor(
    workspace_id: String,
    relative_path: String,
    exact: Option<String>,
) -> comet_proto::SemanticAnchor {
    let target_id = format!("local-workspace:{workspace_id}:{relative_path}");
    comet_proto::SemanticAnchor {
        target_kind: comet_proto::AnchorTargetKind::File,
        target_id,
        file: Some(comet_proto::FileTargetReference::LocalWorkspacePath {
            workspace_id,
            relative_path,
            unknown: Default::default(),
        }),
        byte_range: None,
        exact: exact.map(|text| bounded_anchor_exact(&text)),
        prefix_hash: None,
        suffix_hash: None,
        unknown: Default::default(),
    }
}

/// Resolve a verified host-local grant for one exact agent session. The
/// collaboration principal is project-scoped for display and capability
/// discovery; the grant supplies the narrower deployment/session boundary.
pub fn session_grant_id(
    snapshot: &CollaborationSnapshot,
    session: &comet_proto::AgentSessionRecord,
    required_capability: &str,
    preferred_grant_id: Option<&str>,
    now: i64,
) -> Option<String> {
    let principal = snapshot.principal.as_ref()?;
    if !principal.has_capability(required_capability) {
        return None;
    }
    let permits = |grant: &&comet_proto::CapabilityGrant| {
        grant.principal_subject == principal.subject
            && grant.scope.project_id == principal.project_id
            && grant.scope.session_id.as_deref() == Some(session.session_id.as_str())
            && grant.device_id.as_deref() == Some(session.owner_device_id.as_str())
            && grant.granted_at <= now
            && grant.expires_at.is_some_and(|expires| now < expires)
            && grant.revoked_at.is_none()
            && grant
                .capabilities
                .iter()
                .any(|capability| capability == required_capability)
    };
    preferred_grant_id
        .and_then(|preferred| {
            snapshot
                .grants
                .iter()
                .find(|grant| grant.id == preferred && permits(grant))
        })
        .or_else(|| snapshot.grants.iter().find(permits))
        .map(|grant| grant.id.clone())
}

/// Durable command identity is encoded in the immutable audit id. Keep this
/// mapping centralized so optimistic feedback never falls back to matching an
/// actor, action label, timestamp, or publication order.
pub fn audit_command_id(audit: &comet_proto::AuditEvent) -> Option<&str> {
    audit.id.strip_prefix("audit/").filter(|id| !id.is_empty())
}

pub fn command_audit<'a>(
    command_id: &str,
    audits: &'a [comet_proto::AuditEvent],
) -> Option<&'a comet_proto::AuditEvent> {
    audits
        .iter()
        .rev()
        .find(|audit| audit_command_id(audit) == Some(command_id))
}

pub fn activity_items(snapshot: &CollaborationSnapshot) -> Vec<ActivityItem> {
    let mut items: Vec<ActivityItem> = snapshot
        .publications
        .iter()
        .filter_map(|publication| {
            let (label, detail, tone) = match &publication.value {
                PublicationValue::Audit(audit) => {
                    let tone = match audit.result {
                        AuditResult::Applied => ActivityTone::Success,
                        AuditResult::Rejected | AuditResult::Expired => ActivityTone::Danger,
                        AuditResult::Superseded | AuditResult::Cancelled => ActivityTone::Neutral,
                    };
                    let result = match audit.result {
                        AuditResult::Applied => "applied",
                        AuditResult::Rejected => "rejected",
                        AuditResult::Expired => "expired",
                        AuditResult::Superseded => "superseded",
                        AuditResult::Cancelled => "cancelled",
                    };
                    (
                        format!("{} {result}", audit.action),
                        audit.reason.clone(),
                        tone,
                    )
                }
                PublicationValue::Annotation(annotation) => {
                    let detail =
                        if annotation.anchor.target_kind == comet_proto::AnchorTargetKind::File {
                            format!(
                                "{} · {}\n{}",
                                file_target_source_label(file_target_source(
                                    annotation.anchor.file.as_ref(),
                                )),
                                file_target_label(annotation.anchor.file.as_ref()),
                                annotation.body,
                            )
                        } else {
                            annotation.body.clone()
                        };
                    (
                        format!("Note · {}", annotation_state_label(annotation.state)),
                        Some(detail),
                        if annotation.state == comet_proto::AnnotationState::Orphaned {
                            ActivityTone::Danger
                        } else {
                            ActivityTone::Neutral
                        },
                    )
                }
                PublicationValue::Attachment(attachment) => (
                    "Attached a file".into(),
                    Some(attachment.file_name.clone()),
                    ActivityTone::Neutral,
                ),
                PublicationValue::AgentSession(session) => (
                    "Started an agent".into(),
                    session.model.clone(),
                    ActivityTone::Success,
                ),
                PublicationValue::MessageProvenance(_) => return None,
                PublicationValue::ModelHandoff(handoff) => (
                    "Changed model".into(),
                    Some(format!("{} to {}", handoff.from_model, handoff.to_model)),
                    ActivityTone::Neutral,
                ),
                PublicationValue::Unknown { .. } => return None,
            };
            let target_id = match &publication.value {
                PublicationValue::Annotation(annotation) => {
                    Some(annotation.anchor.target_id.clone())
                }
                _ => None,
            };
            let item_id = match &publication.value {
                PublicationValue::Audit(audit) => audit_command_id(audit)
                    .map(str::to_string)
                    .unwrap_or_else(|| publication.id.clone()),
                _ => publication.id.clone(),
            };
            Some(ActivityItem {
                id: item_id,
                actor: publication.published_by.clone(),
                label,
                detail,
                target_id,
                occurred_at: publication.published_at,
                tone,
            })
        })
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.occurred_at));
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_are_compact_and_stable() {
        assert_eq!(initials("Chris Nguyen"), "CN");
        assert_eq!(initials("kyle@example.com"), "KE");
        assert_eq!(initials(""), "?");
    }

    #[test]
    fn runtime_and_model_keep_missing_state_explicit() {
        assert_eq!(
            runtime_model(Some("Codex"), Some("gpt-5.4")),
            "Codex · gpt-5.4"
        );
        assert_eq!(runtime_model(None, Some("gpt-5.4")), "gpt-5.4");
        assert_eq!(runtime_model(None, None), "Agent not started");
    }

    #[test]
    fn cursor_location_shows_utf8_caret_or_half_open_selection() {
        let mut cursor = comet_proto::ParticipantCursor {
            target_id: "message-1".into(),
            caret: 12,
            selection: None,
            unknown: Default::default(),
        };
        assert_eq!(cursor_location(&cursor), "12");
        cursor.selection = Some(comet_proto::Utf8ByteRange { start: 4, end: 12 });
        assert_eq!(cursor_location(&cursor), "4–12");
    }

    #[test]
    fn file_targets_distinguish_local_and_trusted_artifacts() {
        let local: comet_proto::FileTargetReference = serde_json::from_value(serde_json::json!({
            "kind": "localWorkspacePath",
            "workspaceId": "workspace-1",
            "relativePath": "src/lib.rs"
        }))
        .unwrap();
        let artifact: comet_proto::FileTargetReference =
            serde_json::from_value(serde_json::json!({
                "kind": "scaffoldArtifact",
                "artifactId": "report-123",
                "artifactUri": "ashler://artifact/report-123"
            }))
            .unwrap();
        assert_eq!(file_target_source(Some(&local)), FileTargetSource::Local);
        assert_eq!(
            file_target_source(Some(&artifact)),
            FileTargetSource::AshlerArtifact
        );
        assert_eq!(file_target_label(Some(&local)), "src/lib.rs");
    }

    #[test]
    fn quoted_anchor_keeps_utf8_boundaries_and_context_hashes() {
        let text = "é".repeat(200);
        let anchor = quoted_anchor(
            comet_proto::AnchorTargetKind::Message,
            "message-1".into(),
            &text,
        );
        let range = anchor.byte_range.as_ref().unwrap();
        assert!(text.is_char_boundary(range.end as usize));
        assert_eq!(anchor.exact.as_deref(), Some(&text[..range.end as usize]));
        let empty_hash = comet_doc::anchor_context_hash("");
        assert_eq!(anchor.prefix_hash.as_deref(), Some(empty_hash.as_str()));
        assert!(anchor.suffix_hash.is_some());
    }

    #[test]
    fn local_file_anchor_uses_typed_stable_identity() {
        let anchor = local_file_anchor(
            "checkout-1".into(),
            "src/lib.rs".into(),
            Some("let value = 1;".into()),
        );
        assert_eq!(anchor.target_kind, comet_proto::AnchorTargetKind::File);
        assert_eq!(file_target_label(anchor.file.as_ref()), "src/lib.rs");
        assert_eq!(anchor.exact.as_deref(), Some("let value = 1;"));
    }

    fn audit(command_id: &str, action: &str, occurred_at: i64) -> comet_proto::AuditEvent {
        comet_proto::AuditEvent {
            id: format!("audit/{command_id}"),
            actor_subject: "iap:teammate@example.com".into(),
            actor_device_id: "device-teammate".into(),
            target_id: "session-a".into(),
            action: action.into(),
            occurred_at,
            result: comet_proto::AuditResult::Applied,
            reason: None,
            unknown: Default::default(),
        }
    }

    #[test]
    fn identical_out_of_order_commands_reconcile_only_by_durable_id() {
        let audits = vec![
            audit("command-newer", "respondInput", 20),
            audit("command-older", "respondInput", 10),
        ];
        assert_eq!(
            command_audit("command-newer", &audits).map(audit_command_id),
            Some(Some("command-newer"))
        );
        assert_eq!(
            command_audit("command-older", &audits).map(audit_command_id),
            Some(Some("command-older"))
        );
        assert!(command_audit("command-missing", &audits).is_none());
    }

    #[test]
    fn invitation_preferred_grant_wins_for_the_exact_session() {
        let now = chrono::Utc::now().timestamp_millis();
        let snapshot: CollaborationSnapshot = serde_json::from_value(serde_json::json!({
            "schemaVersion": 2,
            "sessions": [{
                "sessionId": "session-a",
                "chatId": "chat-a",
                "ownerSubject": "iap:owner@example.com",
                "ownerDeviceId": "device-owner",
                "source": "local",
                "createdAt": now
            }],
            "principal": {
                "subject": "iap:invitee@example.com",
                "projectId": "project-a",
                "capabilities": ["session.chat"]
            },
            "grants": [
                {
                    "id": "grant-first",
                    "principalSubject": "iap:invitee@example.com",
                    "scope": {"projectId": "project-a", "sessionId": "session-a"},
                    "capabilities": ["session.chat"],
                    "deviceId": "device-owner",
                    "grantedBy": "iap:owner@example.com",
                    "grantedAt": now - 1,
                    "expiresAt": now + 60_000
                },
                {
                    "id": "grant-invited",
                    "principalSubject": "iap:invitee@example.com",
                    "scope": {"projectId": "project-a", "sessionId": "session-a"},
                    "capabilities": ["session.chat"],
                    "deviceId": "device-owner",
                    "grantedBy": "iap:owner@example.com",
                    "grantedAt": now - 1,
                    "expiresAt": now + 60_000
                }
            ]
        }))
        .unwrap();
        let session = &snapshot.sessions[0];
        assert_eq!(
            session_grant_id(
                &snapshot,
                session,
                comet_proto::CAPABILITY_SESSION_CHAT,
                Some("grant-invited"),
                now,
            )
            .as_deref(),
            Some("grant-invited")
        );
    }
}
