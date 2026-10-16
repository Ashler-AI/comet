use zeron_doc::{SessionDoc, rebuild_thin_doc, reanchor_annotation};
use zeron_proto::*;

#[test]
fn annotations_require_verified_scope_and_survive_rebuild() {
    let scope = CollaborationScope { project_id:"project".into(), deployment_id:Some("canary".into()),
        session_id:Some("session".into()), unknown:Default::default() };
    let metadata = CrewChatMetadata { owner_subject:"owner".into(), scope:scope.clone(),
        session_id:"session".into(), environment:None, agent_route:None };
    let mut principal = CollaborationPrincipal { subject:"owner".into(), email:None,
        project_id:"project".into(), deployment_id:Some("canary".into()), session_id:None,
        capabilities:vec![CAPABILITY_SESSION_ANNOTATE.into()], unknown:Default::default() };
    let doc = SessionDoc::init("session").unwrap();
    let create = AnnotationCommand::Create { id:"note".into(), body:"keep this".into(), anchor:SemanticAnchor {
        target_kind:AnchorTargetKind::Message, target_id:"message".into(), file:None,
        byte_range:Some(Utf8ByteRange { start:0, end:5 }), exact:Some("hello".into()),
        prefix_hash:None, suffix_hash:None, unknown:Default::default(),
    }};
    principal.deployment_id = Some("production".into());
    assert!(doc.apply_annotation_command("create", &create, &principal, None, &metadata, 10).is_err());
    principal.deployment_id = Some("canary".into());
    principal.session_id = Some("session".into());
    assert!(doc.apply_annotation_command("create", &create, &principal, None, &metadata, 10).is_err());
    principal.session_id = None;
    let note = doc.apply_annotation_command("create", &create, &principal, None, &metadata, 10).unwrap();
    assert_eq!(reanchor_annotation(&note, Some("new hello")).state, AnnotationState::Reanchored);
    assert_eq!(reanchor_annotation(&note, None).state, AnnotationState::Orphaned);
    let mut malformed = note.clone();
    malformed.anchor.byte_range = Some(Utf8ByteRange { start: 5, end: 1 });
    malformed.anchor.exact = None;
    assert_eq!(reanchor_annotation(&malformed, Some("hello")).state, AnnotationState::Orphaned);
    let rebuilt = rebuild_thin_doc(&doc).unwrap();
    assert_eq!(rebuilt.doc.annotations().unwrap(), vec![note.clone()]);
    // Crash after snapshot persistence but before the external command finish:
    // reload, then mutate the annotation with a later command.
    let raw = loro::LoroDoc::new();
    raw.import(&rebuilt.doc.export_snapshot().unwrap()).unwrap();
    let restored = SessionDoc::from_doc(raw);
    let edit = AnnotationCommand::Edit { id:"note".into(), body:"later content".into() };
    restored.apply_annotation_command("edit", &edit, &principal, None, &metadata, 20).unwrap();
    assert_eq!(restored.annotation_command_result("create").unwrap(), Some(note.clone()));
    assert_eq!(restored.apply_annotation_command("create", &create, &principal, None, &metadata, 30).unwrap(), note);
    assert!(restored.apply_annotation_command("create", &edit, &principal, None, &metadata, 30).is_err());
    assert_eq!(restored.annotation_command_result("missing").unwrap(), None);
    assert_eq!(restored.annotations().unwrap()[0].body, "later content");
    assert!(restored.apply_annotation_command("resolve", &AnnotationCommand::Resolve { id:"note".into(), resolved:true },
        &principal, None, &metadata, 40).unwrap().resolved_at.is_some());
}
