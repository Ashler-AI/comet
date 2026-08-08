use comet_proto::{FileTargetReference, ParticipantCursor, Utf8ByteRange};

#[test]
fn participant_cursor_validates_utf8_byte_boundaries() {
    let caret = ParticipantCursor {
        target_id: "publication/message-1".into(),
        caret: 3,
        selection: Some(Utf8ByteRange { start: 1, end: 3 }),
        unknown: Default::default(),
    };
    assert!(caret.is_valid_for_text("aéz"));

    let mut split_codepoint = caret.clone();
    split_codepoint.caret = 2;
    assert!(!split_codepoint.is_valid_for_text("aéz"));

    let mut reversed = caret;
    reversed.selection = Some(Utf8ByteRange { start: 3, end: 1 });
    assert!(!reversed.is_bounded());
}

#[test]
fn participant_cursor_preserves_unknown_fields() {
    let cursor: ParticipantCursor = serde_json::from_value(serde_json::json!({
        "targetId": "publication/message-1",
        "caret": 0,
        "futureAffinity": "downstream"
    }))
    .unwrap();
    assert_eq!(
        cursor.unknown.get("futureAffinity"),
        Some(&serde_json::json!("downstream"))
    );
    assert_eq!(
        serde_json::to_value(cursor).unwrap()["futureAffinity"],
        serde_json::json!("downstream")
    );
}

#[test]
fn file_target_reference_preserves_future_metadata() {
    let reference: FileTargetReference = serde_json::from_value(serde_json::json!({
        "kind": "scaffoldArtifact",
        "artifactId": "artifact-1",
        "artifactUri": "scaffold://artifact/artifact-1",
        "futureRegion": "us-central1"
    }))
    .unwrap();
    let FileTargetReference::ScaffoldArtifact { unknown, .. } = &reference else {
        panic!("wrong file target variant");
    };
    assert_eq!(
        unknown.get("futureRegion"),
        Some(&serde_json::json!("us-central1"))
    );
    assert_eq!(
        serde_json::to_value(reference).unwrap()["futureRegion"],
        serde_json::json!("us-central1")
    );
}
