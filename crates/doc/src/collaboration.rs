//! Collaboration invariants that sit above the raw Loro containers.

use comet_proto::{
    AnchorTargetKind, AnnotationState, AttachmentMetadata, FileTargetReference, PublicationRecord,
    PublicationValue, SemanticAnchor, SemanticAnnotation, Utf8ByteRange,
};

use crate::schema::DocError;

pub const MAX_PUBLICATION_JSON_BYTES: usize = 64 * 1024;
pub const MAX_ANNOTATION_BODY_BYTES: usize = 16 * 1024;
pub const MAX_ANCHOR_EXACT_BYTES: usize = 4 * 1024;
pub const MAX_ATTACHMENT_METADATA_ENTRIES: usize = 32;
pub const MAX_ATTACHMENT_METADATA_KEY_BYTES: usize = 64;
pub const MAX_ATTACHMENT_METADATA_VALUE_BYTES: usize = 512;
pub const MAX_ATTACHMENT_FILE_NAME_BYTES: usize = 512;
pub const MAX_ATTACHMENT_MIME_BYTES: usize = 128;
pub const MAX_ATTACHMENT_BLOB_KEY_BYTES: usize = 2 * 1024;
pub const MAX_FILE_REFERENCE_BYTES: usize = 4 * 1024;

pub fn validate_publication(record: &PublicationRecord) -> Result<(), DocError> {
    if record.id.is_empty() || record.id.len() > 256 {
        return Err(DocError::Schema(
            "publication id is empty or too long".into(),
        ));
    }
    let encoded = serde_json::to_vec(record)?;
    if encoded.len() > MAX_PUBLICATION_JSON_BYTES {
        return Err(DocError::Schema(
            "publication metadata exceeds 64 KiB".into(),
        ));
    }
    match &record.value {
        PublicationValue::Annotation(annotation) => validate_annotation(annotation),
        PublicationValue::Attachment(attachment) => validate_attachment(attachment),
        PublicationValue::Audit(event) => {
            if event.id.is_empty()
                || event.id.len() > 256
                || event.actor_subject.is_empty()
                || event.actor_subject.len() > 512
                || event.actor_device_id.len() > 256
                || event.target_id.len() > 256
                || event.action.is_empty()
                || event.action.len() > 128
                || event
                    .reason
                    .as_ref()
                    .is_some_and(|value| value.len() > 2048)
            {
                return Err(DocError::Schema("audit metadata exceeds bounds".into()));
            }
            Ok(())
        }
        PublicationValue::AgentSession(session) => {
            if session.session_id.is_empty()
                || session.session_id.len() > 256
                || session.owner_subject.len() > 512
                || session.owner_device_id.len() > 256
            {
                return Err(DocError::Schema(
                    "agent session metadata exceeds bounds".into(),
                ));
            }
            Ok(())
        }
        PublicationValue::MessageProvenance(provenance) => {
            if provenance.message_id.is_empty()
                || provenance.session_id.is_empty()
                || provenance.author_subject.len() > 512
                || provenance.owner_device_id.len() > 256
            {
                return Err(DocError::Schema("message provenance exceeds bounds".into()));
            }
            Ok(())
        }
        PublicationValue::ModelHandoff(handoff) => {
            if handoff.from_model.len() > 256
                || handoff.to_model.len() > 256
                || handoff
                    .harness_session_id
                    .as_ref()
                    .is_some_and(|value| value.len() > 1024)
            {
                return Err(DocError::Schema(
                    "model handoff metadata exceeds bounds".into(),
                ));
            }
            Ok(())
        }
        PublicationValue::Unknown { kind, .. } => Err(DocError::Schema(format!(
            "unknown collaboration publication kind {kind:?}"
        ))),
    }
}

pub fn validate_annotation(annotation: &SemanticAnnotation) -> Result<(), DocError> {
    if annotation.id.is_empty()
        || annotation.id.len() > 256
        || annotation.author_subject.is_empty()
        || annotation.author_subject.len() > 512
    {
        return Err(DocError::Schema(
            "annotation identity exceeds bounds".into(),
        ));
    }
    if annotation.body.len() > MAX_ANNOTATION_BODY_BYTES {
        return Err(DocError::Schema("annotation body exceeds 16 KiB".into()));
    }
    if annotation.anchor.target_id.is_empty() || annotation.anchor.target_id.len() > 256 {
        return Err(DocError::Schema(
            "annotation target id exceeds bounds".into(),
        ));
    }
    match (&annotation.anchor.target_kind, &annotation.anchor.file) {
        (
            AnchorTargetKind::File,
            Some(FileTargetReference::LocalWorkspacePath {
                workspace_id,
                relative_path,
                ..
            }),
        ) => {
            if workspace_id.is_empty()
                || workspace_id.len() > 256
                || relative_path.is_empty()
                || relative_path.len() > MAX_FILE_REFERENCE_BYTES
                || relative_path.starts_with('/')
                || relative_path.contains('\\')
                || relative_path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
            {
                return Err(DocError::Schema(
                    "local file anchor is not a bounded workspace-relative path".into(),
                ));
            }
        }
        (
            AnchorTargetKind::File,
            Some(FileTargetReference::ScaffoldArtifact {
                artifact_id,
                artifact_uri,
                ..
            }),
        ) => {
            if artifact_id.is_empty()
                || artifact_id.len() > 256
                || artifact_uri.as_ref().is_some_and(|uri| {
                    uri.len() > MAX_FILE_REFERENCE_BYTES
                        || !(uri.starts_with("ashler://artifact/")
                            || uri.starts_with("scaffold://artifact/"))
                        || uri.contains('?')
                        || uri.contains('#')
                })
            {
                return Err(DocError::Schema(
                    "artifact file anchor is invalid or may contain a credential".into(),
                ));
            }
        }
        (AnchorTargetKind::File, None) => {
            return Err(DocError::Schema(
                "file anchor requires a typed file reference".into(),
            ));
        }
        (_, Some(_)) => {
            return Err(DocError::Schema(
                "non-file anchor cannot carry a file reference".into(),
            ));
        }
        _ => {}
    }
    if annotation
        .anchor
        .exact
        .as_ref()
        .is_some_and(|value| value.len() > MAX_ANCHOR_EXACT_BYTES)
    {
        return Err(DocError::Schema(
            "annotation anchor quote exceeds 4 KiB".into(),
        ));
    }
    validate_anchor_range(&annotation.anchor, None)
}

pub fn validate_attachment(attachment: &AttachmentMetadata) -> Result<(), DocError> {
    if attachment.file_name.is_empty()
        || attachment.file_name.len() > MAX_ATTACHMENT_FILE_NAME_BYTES
        || attachment.mime_type.len() > MAX_ATTACHMENT_MIME_BYTES
        || attachment.blob_key.is_empty()
        || attachment.blob_key.len() > MAX_ATTACHMENT_BLOB_KEY_BYTES
        || attachment.metadata.len() > MAX_ATTACHMENT_METADATA_ENTRIES
    {
        return Err(DocError::Schema(
            "attachment metadata exceeds bounds".into(),
        ));
    }
    if attachment.metadata.iter().any(|(key, value)| {
        key.len() > MAX_ATTACHMENT_METADATA_KEY_BYTES
            || value.len() > MAX_ATTACHMENT_METADATA_VALUE_BYTES
    }) {
        return Err(DocError::Schema(
            "attachment metadata entry exceeds bounds".into(),
        ));
    }
    Ok(())
}

fn validate_anchor_range(anchor: &SemanticAnchor, text: Option<&str>) -> Result<(), DocError> {
    let Some(range) = &anchor.byte_range else {
        return Ok(());
    };
    if range.start > range.end {
        return Err(DocError::Schema("annotation byte range is reversed".into()));
    }
    if let Some(text) = text {
        let start = range.start as usize;
        let end = range.end as usize;
        if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return Err(DocError::Schema(
                "annotation byte range is not valid UTF-8".into(),
            ));
        }
    }
    Ok(())
}

/// Stable, implementation-independent FNV-1a context hash. This is an anchor checksum, not a
/// security primitive. Hex output keeps Rust/Worker peers deterministic.
pub fn anchor_context_hash(text: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn context_before(text: &str, start: usize) -> &str {
    const WINDOW: usize = 64;
    let mut at = start.saturating_sub(WINDOW);
    while at < start && !text.is_char_boundary(at) {
        at += 1;
    }
    &text[at..start]
}

fn context_after(text: &str, end: usize) -> &str {
    const WINDOW: usize = 64;
    let mut at = (end + WINDOW).min(text.len());
    while at > end && !text.is_char_boundary(at) {
        at -= 1;
    }
    &text[end..at]
}

fn context_matches(anchor: &SemanticAnchor, text: &str, start: usize, end: usize) -> bool {
    anchor
        .prefix_hash
        .as_deref()
        .is_none_or(|hash| anchor_context_hash(context_before(text, start)) == hash)
        && anchor
            .suffix_hash
            .as_deref()
            .is_none_or(|hash| anchor_context_hash(context_after(text, end)) == hash)
}

/// Re-anchor without deleting or rewriting annotation content.
///
/// Order is fixed: valid original UTF-8 range + quote, then all exact quote occurrences whose
/// context hashes match (lowest byte offset wins), then orphaned. The stable target id itself is
/// supplied by the caller selecting `target_text`; a missing target calls this with `None`.
pub fn reanchor_annotation(
    annotation: &SemanticAnnotation,
    target_text: Option<&str>,
) -> SemanticAnnotation {
    let mut result = annotation.clone();
    let Some(text) = target_text else {
        result.state = AnnotationState::Orphaned;
        return result;
    };
    let quote = annotation.anchor.exact.as_deref();
    if let Some(range) = &annotation.anchor.byte_range {
        let start = range.start as usize;
        let end = range.end as usize;
        if end <= text.len()
            && text.is_char_boundary(start)
            && text.is_char_boundary(end)
            && quote.is_none_or(|expected| &text[start..end] == expected)
        {
            result.state = AnnotationState::Anchored;
            return result;
        }
    }
    if let Some(quote) = quote.filter(|value| !value.is_empty()) {
        let mut search_from = 0;
        while search_from <= text.len() {
            let Some(relative) = text[search_from..].find(quote) else {
                break;
            };
            let start = search_from + relative;
            let end = start + quote.len();
            if context_matches(&annotation.anchor, text, start, end) {
                result.anchor.byte_range = Some(Utf8ByteRange {
                    start: start as u32,
                    end: end as u32,
                });
                result.state = AnnotationState::Reanchored;
                return result;
            }
            search_from = end.max(start + 1);
        }
    }
    result.state = AnnotationState::Orphaned;
    result
}

/// Re-anchor file annotations in an ephemeral collaboration projection.
///
/// Publication records remain immutable in the document: only the projected annotation value is
/// replaced, preserving its stable annotation id, author, timestamps, body, resolution state, and
/// publication audit metadata. The resolver is keyed by the complete typed anchor rather than a
/// selected row or display target, so duplicate anchors cannot redirect a projection.
pub fn reanchor_projected_file_annotations(
    snapshot: &mut comet_proto::CollaborationSnapshot,
    mut target_text: impl FnMut(&SemanticAnchor) -> Option<String>,
) {
    for publication in &mut snapshot.publications {
        let PublicationValue::Annotation(annotation) = &mut publication.value else {
            continue;
        };
        if annotation.anchor.target_kind != AnchorTargetKind::File {
            continue;
        }
        let text = target_text(&annotation.anchor);
        *annotation = reanchor_annotation(annotation, text.as_deref());
    }
}
