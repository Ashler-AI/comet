use std::collections::BTreeMap;

use comet_doc::{
    SessionDoc, anchor_context_hash, reanchor_annotation, reanchor_projected_file_annotations,
};
use comet_proto::{
    AgentSessionRecord, AgentSessionSource, AnchorTargetKind, AnnotationState, AttachmentMetadata,
    AuditEvent, AuditResult, COLLABORATION_SCHEMA_VERSION, FileTargetReference, MessageProvenance,
    ModelHandoff, PublicationRecord, PublicationValue, SemanticAnchor, SemanticAnnotation,
    SessionStatus, Utf8ByteRange,
};

fn publication(id: &str, value: PublicationValue) -> PublicationRecord {
    PublicationRecord {
        id: id.into(),
        schema_version: COLLABORATION_SCHEMA_VERSION,
        published_at: 100,
        published_by: "iap:chris@example.com".into(),
        value,
        unknown: BTreeMap::new(),
    }
}

#[test]
fn annotation_round_trips_and_reanchors_deterministically() {
    let before = "alpha selected omega";
    let annotation = SemanticAnnotation {
        id: "annotation-1".into(),
        author_subject: "iap:kyle@example.com".into(),
        body: "Check this assumption".into(),
        anchor: SemanticAnchor {
            target_kind: AnchorTargetKind::Message,
            target_id: "message-1".into(),
            file: None,
            byte_range: Some(Utf8ByteRange { start: 6, end: 14 }),
            exact: Some("selected".into()),
            prefix_hash: None,
            suffix_hash: None,
            unknown: BTreeMap::new(),
        },
        state: AnnotationState::Anchored,
        created_at: 10,
        resolved_at: None,
        unknown: BTreeMap::new(),
    };
    let doc = SessionDoc::init("thread-1").unwrap();
    doc.append_publication(&publication(
        "publication-annotation-1",
        PublicationValue::Annotation(annotation.clone()),
    ))
    .unwrap();
    let snapshot = doc.export_snapshot().unwrap();
    let restored = loro::LoroDoc::new();
    restored.import(&snapshot).unwrap();
    let restored = SessionDoc::from_doc(restored);
    assert_eq!(restored.read_publications().unwrap().len(), 1);

    let after = "prefix alpha selected omega suffix";
    let anchored = reanchor_annotation(&annotation, Some(after));
    assert_eq!(anchored.state, AnnotationState::Reanchored);
    assert_eq!(
        anchored.anchor.byte_range,
        Some(Utf8ByteRange { start: 13, end: 21 })
    );
    assert_eq!(
        reanchor_annotation(&annotation, None).state,
        AnnotationState::Orphaned
    );
    assert_eq!(before.as_bytes()[6..14], *b"selected");
    assert_eq!(
        anchor_context_hash("context"),
        anchor_context_hash("context")
    );
}

#[test]
fn collaboration_projection_reanchors_moved_file_text_without_rewriting_identity_or_audit() {
    let annotation = SemanticAnnotation {
        id: "annotation-file-move".into(),
        author_subject: "accounts.google.com:subject-42".into(),
        body: "Keep this review metadata".into(),
        anchor: SemanticAnchor {
            target_kind: AnchorTargetKind::File,
            target_id: "local-workspace:workspace-1:src/lib.rs".into(),
            file: Some(FileTargetReference::LocalWorkspacePath {
                workspace_id: "workspace-1".into(),
                relative_path: "src/lib.rs".into(),
                unknown: BTreeMap::new(),
            }),
            byte_range: Some(Utf8ByteRange { start: 6, end: 14 }),
            exact: Some("selected".into()),
            prefix_hash: None,
            suffix_hash: None,
            unknown: BTreeMap::new(),
        },
        state: AnnotationState::Anchored,
        created_at: 17,
        resolved_at: Some(23),
        unknown: BTreeMap::from([("auditTag".into(), serde_json::json!("retained"))]),
    };
    let doc = SessionDoc::init("thread-file-move").unwrap();
    let record = PublicationRecord {
        id: "annotation/annotation-file-move/edit/command-7".into(),
        schema_version: COLLABORATION_SCHEMA_VERSION,
        published_at: 29,
        published_by: "accounts.google.com:subject-42".into(),
        value: PublicationValue::Annotation(annotation.clone()),
        unknown: BTreeMap::from([("traceId".into(), serde_json::json!("trace-7"))]),
    };
    doc.append_publication(&record).unwrap();

    let mut projection = doc.collaboration_snapshot().unwrap();
    reanchor_projected_file_annotations(&mut projection, |anchor| {
        (anchor.target_id == "local-workspace:workspace-1:src/lib.rs")
            .then(|| "prefix alpha selected omega suffix".to_string())
    });

    let projected = &projection.publications[0];
    assert_eq!(projected.id, record.id);
    assert_eq!(projected.published_at, record.published_at);
    assert_eq!(projected.published_by, record.published_by);
    assert_eq!(projected.unknown, record.unknown);
    let PublicationValue::Annotation(projected_annotation) = &projected.value else {
        panic!("annotation projection");
    };
    assert_eq!(projected_annotation.id, annotation.id);
    assert_eq!(
        projected_annotation.author_subject,
        annotation.author_subject
    );
    assert_eq!(projected_annotation.body, annotation.body);
    assert_eq!(projected_annotation.created_at, annotation.created_at);
    assert_eq!(projected_annotation.resolved_at, annotation.resolved_at);
    assert_eq!(projected_annotation.unknown, annotation.unknown);
    assert_eq!(projected_annotation.state, AnnotationState::Reanchored);
    assert_eq!(
        projected_annotation.anchor.byte_range,
        Some(Utf8ByteRange { start: 13, end: 21 })
    );

    let persisted = doc.read_publications().unwrap();
    assert_eq!(persisted, vec![record]);
}

#[test]
fn structured_attachment_metadata_is_bounded_and_survives_snapshot() {
    let attachment = AttachmentMetadata {
        id: "attachment-1".into(),
        file_name: "trace.json".into(),
        mime_type: "application/json".into(),
        size_bytes: 42,
        blob_key: "sessions/thread-1/attachment-1".into(),
        sha256: Some("a".repeat(64)),
        created_by: "iap:chris@example.com".into(),
        created_at: 20,
        metadata: BTreeMap::from([("purpose".into(), "debug-log".into())]),
        unknown: BTreeMap::from([("futureField".into(), serde_json::json!(true))]),
    };
    let doc = SessionDoc::init("thread-1").unwrap();
    doc.append_publication(&publication(
        "publication-attachment-1",
        PublicationValue::Attachment(attachment.clone()),
    ))
    .unwrap();
    let [record] = doc.read_publications().unwrap().try_into().unwrap();
    assert_eq!(record.value, PublicationValue::Attachment(attachment));

    let mut oversized = match record.value {
        PublicationValue::Attachment(value) => value,
        _ => unreachable!(),
    };
    oversized.metadata.insert("too-big".into(), "x".repeat(513));
    assert!(
        doc.append_publication(&publication(
            "oversized",
            PublicationValue::Attachment(oversized)
        ))
        .is_err()
    );
}

#[test]
fn reconnect_and_model_handoff_keep_both_sessions_publications() {
    let doc = SessionDoc::init("shared-thread").unwrap();
    for (id, owner, device, source) in [
        (
            "chris-session",
            "iap:chris@example.com",
            "mac-chris",
            AgentSessionSource::Local,
        ),
        (
            "kyle-session",
            "iap:kyle@example.com",
            "scaffold-kyle",
            AgentSessionSource::Scaffold,
        ),
    ] {
        doc.append_publication(&publication(
            &format!("session/{id}"),
            PublicationValue::AgentSession(Box::new(AgentSessionRecord {
                session_id: id.into(),
                chat_id: "shared-thread".into(),
                owner_subject: owner.into(),
                owner_device_id: device.into(),
                source,
                environment: None,
                harness: None,
                model: Some("model-a".into()),
                harness_session_id: None,
                status: Some(SessionStatus::Working),
                updated_at: Some(30),
                created_at: 30,
                unknown: BTreeMap::new(),
            })),
        ))
        .unwrap();
    }
    doc.append_publication(&publication(
        "provenance/message-kyle",
        PublicationValue::MessageProvenance(MessageProvenance {
            message_id: "message-kyle".into(),
            session_id: "kyle-session".into(),
            author_subject: "iap:kyle@example.com".into(),
            owner_device_id: "scaffold-kyle".into(),
            model: Some("model-a".into()),
            source: AgentSessionSource::Scaffold,
            unknown: BTreeMap::new(),
        }),
    ))
    .unwrap();
    doc.append_publication(&publication(
        "handoff/kyle-1",
        PublicationValue::ModelHandoff(ModelHandoff {
            id: "handoff/kyle-1".into(),
            from_model: "model-a".into(),
            to_model: "model-b".into(),
            harness_session_id: Some("harness-kyle".into()),
            after_publication_id: Some("provenance/message-kyle".into()),
            created_by: "iap:kyle@example.com".into(),
            created_at: 40,
            unknown: BTreeMap::new(),
        }),
    ))
    .unwrap();
    doc.append_publication(&publication(
        "audit/cmd-kyle",
        PublicationValue::Audit(AuditEvent {
            id: "audit/cmd-kyle".into(),
            actor_subject: "iap:chris@example.com".into(),
            actor_device_id: "mac-chris".into(),
            target_id: "kyle-session".into(),
            action: "pause".into(),
            occurred_at: 50,
            result: AuditResult::Rejected,
            reason: Some("not owner".into()),
            unknown: BTreeMap::new(),
        }),
    ))
    .unwrap();

    let before = doc.read_publications().unwrap();
    assert!(!doc.append_publication(&before[0]).unwrap());
    let bytes = doc.export_snapshot().unwrap();
    let restored = loro::LoroDoc::new();
    restored.import(&bytes).unwrap();
    let restored = SessionDoc::from_doc(restored);
    let snapshot = restored.collaboration_snapshot().unwrap();
    assert_eq!(snapshot.sessions.len(), 2);
    assert_eq!(snapshot.publications.len(), before.len());
    let audit = snapshot
        .publications
        .iter()
        .find(|item| item.id == "audit/cmd-kyle")
        .unwrap();
    assert!(matches!(&audit.value, PublicationValue::Audit(event)
        if event.actor_subject == "iap:chris@example.com" && event.target_id == "kyle-session"));
}

#[test]
fn file_annotations_use_stable_credential_free_references() {
    let doc = SessionDoc::init("thread-file").unwrap();
    let annotation = |file| SemanticAnnotation {
        id: "annotation-file".into(),
        author_subject: "iap:chris@example.com".into(),
        body: "Check this file".into(),
        anchor: SemanticAnchor {
            target_kind: AnchorTargetKind::File,
            target_id: "file-target-1".into(),
            file: Some(file),
            byte_range: None,
            exact: None,
            prefix_hash: None,
            suffix_hash: None,
            unknown: BTreeMap::new(),
        },
        state: AnnotationState::Anchored,
        created_at: 10,
        resolved_at: None,
        unknown: BTreeMap::new(),
    };
    let local = annotation(FileTargetReference::LocalWorkspacePath {
        workspace_id: "workspace-1".into(),
        relative_path: "src/lib.rs".into(),
        unknown: BTreeMap::new(),
    });
    assert!(
        doc.append_publication(&publication(
            "publication-file",
            PublicationValue::Annotation(local),
        ))
        .unwrap()
    );

    let signed_url = annotation(FileTargetReference::ScaffoldArtifact {
        artifact_id: "artifact-1".into(),
        artifact_uri: Some("scaffold://artifact/artifact-1?token=secret".into()),
        unknown: BTreeMap::new(),
    });
    assert!(
        doc.append_publication(&publication(
            "publication-file-secret",
            PublicationValue::Annotation(signed_url),
        ))
        .is_err()
    );
}
