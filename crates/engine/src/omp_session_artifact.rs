//! Trusted capture of an OMP native session file for exact platform handoff.
//!
//! The capture is intentionally local-controller only. It opens an already
//! discovered path without following a final symlink, rejects multiply-linked
//! files, bounds the read, and validates identity/metadata before and after the
//! read so bytes cannot silently change underneath the capture.

use base64::Engine as _;
use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
use std::path::Path;

use comet_proto::OmpSessionArtifact;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

use crate::EngineError;

/// Maximum native OMP session accepted for handoff (64 MiB).
pub const MAX_OMP_SESSION_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

const MAX_OMP_SESSION_HEADER_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) struct CapturedOmpSessionFile {
    file: NamedTempFile,
    pub native_session_id: String,
    pub cwd: String,
    pub storage_relative_path: String,
    pub sha256: String,
    pub byte_count: u64,
}

impl CapturedOmpSessionFile {
    pub fn reopen(&self) -> io::Result<File> {
        self.file.reopen()
    }

    /// Only the captured prior conversation is normalized. The new remote prompt
    /// is queued separately and never passes through this fail-open boundary.
    pub(crate) fn prepare_historical_attachments(
        mut self,
        blob_dir: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Self, EngineError> {
        use std::io::{BufRead, BufReader};
        let mut output = NamedTempFile::new()?;
        let mut digest = Sha256::new();
        let mut byte_count = 0_u64;
        let mut missing = 0_usize;
        let mut expansion_budget = MAX_OMP_SESSION_ARTIFACT_BYTES.saturating_sub(self.byte_count);
        for line in BufReader::new(self.reopen()?).split(b'\n') {
            check_cancelled(cancellation)?;
            let line = line?;
            if line.is_empty() {
                continue;
            }
            let mut entry: Value = serde_json::from_slice(&line)
                .map_err(|_| invalid("OMP historical session contains invalid JSON"))?;
            let changed = prepare_history_entry(
                &mut entry,
                blob_dir,
                &mut expansion_budget,
                &mut missing,
                cancellation,
            )?;
            let bytes = if changed {
                serde_json::to_vec(&entry)
                    .map_err(|_| invalid("Could not encode prepared OMP history"))?
            } else {
                line
            };
            byte_count = byte_count.saturating_add(bytes.len() as u64 + 1);
            if byte_count > MAX_OMP_SESSION_ARTIFACT_BYTES {
                return Err(invalid("Prepared OMP history exceeds capture limit"));
            }
            output.write_all(&bytes)?;
            output.write_all(b"\n")?;
            digest.update(&bytes);
            digest.update(b"\n");
        }
        output.flush()?;
        if missing > 0 {
            tracing::warn!(
                missing_attachments = missing,
                "Crew handoff preserved unavailable historical attachments as text markers"
            );
        }
        self.file = output;
        self.byte_count = byte_count;
        self.sha256 = format!("{:x}", digest.finalize());
        Ok(self)
    }
}

pub(crate) fn capture_omp_session_file(
    path: &Path,
    storage_relative_path: &Path,
    expected_native_session_id: &str,
    expected_cwd: &str,
    cancellation: &CancellationToken,
) -> Result<CapturedOmpSessionFile, EngineError> {
    if expected_native_session_id.trim().is_empty() || expected_cwd.trim().is_empty() {
        return Err(invalid("OMP native session id and cwd are required"));
    }
    let storage_relative_path = validate_storage_relative_path(storage_relative_path)?;
    check_cancelled(cancellation)?;

    let mut input = open_regular_nofollow(path)?;
    let before = input.metadata()?;
    validate_metadata(&before, MAX_OMP_SESSION_ARTIFACT_BYTES)?;
    let mut output = NamedTempFile::new()?;
    let mut digest = Sha256::new();
    let mut byte_count = 0_u64;
    let mut header = Vec::with_capacity(MAX_OMP_SESSION_HEADER_BYTES.min(before.len() as usize));
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        check_cancelled(cancellation)?;
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        byte_count = byte_count.saturating_add(read as u64);
        if byte_count > MAX_OMP_SESSION_ARTIFACT_BYTES || byte_count > before.len() {
            return Err(invalid("OMP session file changed during capture"));
        }
        if header.len() < MAX_OMP_SESSION_HEADER_BYTES {
            let retained = read.min(MAX_OMP_SESSION_HEADER_BYTES - header.len());
            header.extend_from_slice(&buffer[..retained]);
        }
        digest.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }

    if byte_count != before.len() {
        return Err(invalid("OMP session file changed during capture"));
    }
    let after = input.metadata()?;
    validate_metadata(&after, MAX_OMP_SESSION_ARTIFACT_BYTES)?;
    if !same_file_state(&before, &after) {
        return Err(invalid("OMP session file changed during capture"));
    }
    let complete_header_len = header
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .ok_or_else(|| invalid("OMP session header exceeds the capture identity window"))?;
    let (native_session_id, cwd) = parse_session_header(&header[..complete_header_len])?;
    if native_session_id != expected_native_session_id || cwd != expected_cwd {
        return Err(invalid(
            "OMP session header does not match the discovered native id and cwd",
        ));
    }
    output.as_file_mut().flush()?;
    check_cancelled(cancellation)?;

    Ok(CapturedOmpSessionFile {
        file: output,
        native_session_id,
        cwd,
        storage_relative_path,
        sha256: format!("{:x}", digest.finalize()),
        byte_count,
    })
}

fn prepare_history_entry(
    entry: &mut Value,
    blob_dir: &Path,
    budget: &mut u64,
    missing: &mut usize,
    cancellation: &CancellationToken,
) -> Result<bool, EngineError> {
    if entry.get("type").and_then(Value::as_str) == Some("compaction") {
        let mut changed = false;
        let before_missing = *missing;
        if let Some(frames) = entry
            .pointer_mut("/preserveData/snapcompact/frames")
            .and_then(Value::as_array_mut)
        {
            for frame in frames.iter_mut() {
                check_cancelled(cancellation)?;
                let data = frame.get("data").and_then(Value::as_str).unwrap_or("");
                if valid_attachment_base64(data) {
                    continue;
                }
                if let Some(hash) = data.strip_prefix("blob:sha256:").filter(|hash| {
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                }) {
                    if let Some(bytes) = read_historical_blob(blob_dir, hash, *budget) {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                        *budget -= encoded.len() as u64 + 128;
                        frame["data"] = Value::String(encoded);
                        changed = true;
                        continue;
                    }
                }
                let removed = serde_json::to_vec(frame)
                    .map_err(|_| invalid("Could not size archive frame"))?
                    .len() as u64;
                *budget = budget.saturating_add(removed);
                *frame = Value::Null;
                *missing += 1;
                changed = true;
            }
            frames.retain(|frame| !frame.is_null());
        }
        if *missing > before_missing {
            let marker =
                "[Historical archive image unavailable. Do not infer missing archive contents.]\n";
            if *budget < marker.len() as u64 {
                return Err(invalid("OMP history has no room for archive warning"));
            }
            *budget -= marker.len() as u64;
            let summary = entry.get("summary").and_then(Value::as_str).unwrap_or("");
            entry["summary"] = Value::String(format!("{marker}{summary}"));
        }
        return Ok(changed);
    }
    if entry.get("type").and_then(Value::as_str) != Some("message") {
        return Ok(false);
    }
    let Some(message) = entry.get_mut("message") else {
        return Ok(false);
    };
    if !matches!(
        message.get("role").and_then(Value::as_str),
        Some("user" | "assistant" | "developer" | "toolResult")
    ) {
        return Ok(false);
    }
    let mut changed = false;
    if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
        for block in content {
            changed |= prepare_history_block(block, blob_dir, budget, missing, cancellation)?;
        }
    }
    if let Some(payload) = message.get_mut("providerPayload") {
        if payload.get("type").and_then(Value::as_str) == Some("openaiResponsesHistory") {
            if let Some(items) = payload.get_mut("items").and_then(Value::as_array_mut) {
                for item in items {
                    if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
                        if let Some(output) = item.get_mut("output").and_then(Value::as_array_mut) {
                            for block in output {
                                changed |= prepare_history_block(
                                    block,
                                    blob_dir,
                                    budget,
                                    missing,
                                    cancellation,
                                )?;
                            }
                        }
                        continue;
                    }
                    if item
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind != "message")
                    {
                        continue;
                    }
                    if !matches!(
                        item.get("role").and_then(Value::as_str),
                        Some("user" | "assistant" | "developer")
                    ) {
                        continue;
                    }
                    if let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) {
                        for block in content {
                            changed |= prepare_history_block(
                                block,
                                blob_dir,
                                budget,
                                missing,
                                cancellation,
                            )?;
                        }
                    }
                }
            }
        }
    }
    Ok(changed)
}

fn prepare_history_block(
    block: &mut Value,
    blob_dir: &Path,
    budget: &mut u64,
    missing: &mut usize,
    cancellation: &CancellationToken,
) -> Result<bool, EngineError> {
    check_cancelled(cancellation)?;
    let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
    let (pointer, url) = match kind {
        "image" | "document" if block.get("source").is_some() => ("/source/data", false),
        "image" | "audio" | "file" => ("/data", false),
        "input_image" => ("/image_url", true),
        "image_url" => ("/image_url/url", true),
        "input_file" => ("/file_data", true),
        "input_audio" => ("/input_audio/data", false),
        _ => return Ok(false),
    };
    let marker_kind = if kind.starts_with("input_") {
        "input_text"
    } else {
        "text"
    };
    if let Some(field) = block.pointer_mut(pointer) {
        if let Some(data) = field.as_str() {
            let encoded = if url {
                data.split_once(";base64,")
                    .filter(|_| data.starts_with("data:"))
                    .map(|(_, bytes)| bytes)
            } else {
                Some(data)
            };
            if encoded.is_some_and(valid_attachment_base64) {
                return Ok(false);
            }
            let reference = encoded.unwrap_or(data);
            if let Some(hash) = reference.strip_prefix("blob:sha256:") {
                if hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    if let Some(bytes) = read_historical_blob(blob_dir, hash, *budget) {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                        *budget = budget.saturating_sub(encoded.len() as u64 + 128);
                        let replacement = if url {
                            let media_type = data
                                .strip_prefix("data:")
                                .and_then(|rest| rest.split_once(';'))
                                .map(|(mime, _)| mime)
                                .unwrap_or("image/png");
                            format!("data:{media_type};base64,{encoded}")
                        } else {
                            encoded
                        };
                        *field = Value::String(replacement);
                        return Ok(true);
                    }
                }
            }
        }
    }
    let original_size = serde_json::to_vec(block)
        .map_err(|_| invalid("Could not size historical attachment"))?
        .len() as u64;
    let mut marker = serde_json::json!({"type": marker_kind, "text": "[Historical attachment unavailable: its data or URL was not carried into this session. Do not infer its contents; request the attachment if needed.]"});
    let mut marker_size = serde_json::to_vec(&marker)
        .map_err(|_| invalid("Could not size attachment marker"))?
        .len() as u64;
    if marker_size.saturating_sub(original_size) > *budget {
        marker["text"] = Value::String("[Historical attachment unavailable]".into());
        marker_size = serde_json::to_vec(&marker)
            .map_err(|_| invalid("Could not size attachment marker"))?
            .len() as u64;
    }
    let expansion = marker_size.saturating_sub(original_size);
    if expansion > *budget {
        return Err(invalid(
            "OMP history has no room for unavailable attachment markers",
        ));
    }
    *budget -= expansion;
    *block = marker;
    *missing += 1;
    Ok(true)
}

fn read_historical_blob(blob_dir: &Path, hash: &str, budget: u64) -> Option<Vec<u8>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(blob_dir.join(hash)).ok()?;
    let before = file.metadata().ok()?;
    let encoded_size = before
        .len()
        .checked_add(2)?
        .checked_div(3)?
        .checked_mul(4)?
        .checked_add(128)?;
    if !before.is_file() || encoded_size > budget {
        return None;
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(before.len().checked_add(1)?)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 != before.len()
        || !same_file_state(&before, &file.metadata().ok()?)
        || hex_sha256(&bytes) != hash
    {
        return None;
    }
    Some(bytes)
}

fn valid_attachment_base64(encoded: &str) -> bool {
    if encoded.is_empty() || encoded.len() % 4 != 0 {
        return false;
    }
    // Validate in fixed-size chunks; avoid decoding multi-megabyte images into
    // another allocation merely to check that references are actually bytes.
    let mut decoded = [0_u8; 768];
    let chunks = encoded.as_bytes().chunks(1024);
    let count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        if index + 1 < count && chunk.contains(&b'=') {
            return false;
        }
        if base64::engine::general_purpose::STANDARD
            .decode_slice(chunk, &mut decoded)
            .is_err()
        {
            return false;
        }
    }
    true
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), EngineError> {
    if cancellation.is_cancelled() {
        return Err(invalid("OMP session capture was cancelled"));
    }
    Ok(())
}

/// Capture one exact OMP JSONL session file.
///
/// `expected_native_session_id` and `expected_cwd` must match the OMP session
/// header stored in the captured bytes. Callers must supply the concrete path
/// found by local OMP discovery; the path is never included in the result.
pub(crate) fn capture_omp_session_artifact(
    path: &Path,
    storage_relative_path: &Path,
    expected_native_session_id: &str,
    expected_cwd: &str,
) -> Result<OmpSessionArtifact, EngineError> {
    capture_with_limit(
        path,
        storage_relative_path,
        expected_native_session_id,
        expected_cwd,
        MAX_OMP_SESSION_ARTIFACT_BYTES,
    )
}

fn capture_with_limit(
    path: &Path,
    storage_relative_path: &Path,
    expected_native_session_id: &str,
    expected_cwd: &str,
    limit: u64,
) -> Result<OmpSessionArtifact, EngineError> {
    if expected_native_session_id.trim().is_empty() || expected_cwd.trim().is_empty() {
        return Err(invalid("OMP native session id and cwd are required"));
    }
    let storage_relative_path = validate_storage_relative_path(storage_relative_path)?;

    let mut file = open_regular_nofollow(path)?;
    let before = file.metadata()?;
    validate_metadata(&before, limit)?;

    let capacity = usize::try_from(before.len())
        .map_err(|_| invalid("OMP session file size is not addressable"))?;
    let mut bytes = Vec::with_capacity(capacity);
    std::io::Read::by_ref(&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("OMP session file exceeds the capture limit"));
    }

    let after = file.metadata()?;
    validate_metadata(&after, limit)?;
    if !same_file_state(&before, &after) || after.len() != bytes.len() as u64 {
        return Err(invalid("OMP session file changed during capture"));
    }

    let (native_session_id, cwd) = parse_session_header(&bytes)?;
    if native_session_id != expected_native_session_id || cwd != expected_cwd {
        return Err(invalid(
            "OMP session header does not match the discovered native id and cwd",
        ));
    }

    Ok(OmpSessionArtifact {
        native_session_id,
        cwd,
        storage_relative_path,
        sha256: hex_sha256(&bytes),
        byte_count: bytes.len() as u64,
        bytes,
    })
}

fn validate_storage_relative_path(path: &Path) -> Result<String, EngineError> {
    use std::path::Component;

    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(invalid("OMP session storage path must be relative"));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .filter(|part| !part.is_empty())
                    .ok_or_else(|| invalid("OMP session storage path must be UTF-8"))?;
                parts.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(invalid("OMP session storage path may not escape its root"));
            }
        }
    }
    if parts.is_empty() || !parts.last().is_some_and(|part| part.ends_with(".jsonl")) {
        return Err(invalid("OMP session storage path must name a JSONL file"));
    }
    Ok(parts.join("/"))
}

fn invalid(message: &str) -> EngineError {
    EngineError::Other(message.into())
}

fn validate_metadata(metadata: &Metadata, limit: u64) -> Result<(), EngineError> {
    if !metadata.is_file() {
        return Err(invalid("OMP session path is not a regular file"));
    }
    if metadata.len() > limit {
        return Err(invalid("OMP session file exceeds the capture limit"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return Err(invalid("OMP session file must have exactly one hard link"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_regular_nofollow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_regular_nofollow(path: &Path) -> io::Result<File> {
    // Windows stable Rust has no OpenOptions no-follow flag. Fail closed when
    // the final component is visibly a symlink, then validate the open handle.
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "OMP session symlinks are not accepted",
        ));
    }
    File::open(path)
}

#[cfg(unix)]
fn same_file_state(before: &Metadata, after: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
        && before.nlink() == after.nlink()
}

#[cfg(not(unix))]
fn same_file_state(before: &Metadata, after: &Metadata) -> bool {
    before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
        && before.created().ok() == after.created().ok()
}

fn parse_session_header(bytes: &[u8]) -> Result<(String, String), EngineError> {
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_slice(line)
            .map_err(|_| invalid("OMP session file contains invalid JSONL"))?;
        if value.get("type").and_then(Value::as_str) != Some("session") {
            continue;
        }
        let native_session_id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("OMP session header has no native id"))?;
        let cwd = value
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("OMP session header has no cwd"))?;
        return Ok((native_session_id.into(), cwd.into()));
    }
    Err(invalid("OMP session file has no session header"))
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn historical_attachments_snapcompact_frames_hydrate_or_disclose_missing_archive() {
        let temp = TempDir::new().unwrap();
        let bytes = b"archive image bytes";
        let hash = hex_sha256(bytes);
        std::fs::write(temp.path().join(&hash), bytes).unwrap();
        let mut entry = serde_json::json!({"type":"compaction","summary":"prior context","preserveData":{"snapcompact":{"frames":[
            {"data":format!("blob:sha256:{hash}"),"mimeType":"image/png","cols":196,"rows":71,"detail":"original"},
            {"data":"blob:sha256:missing","mimeType":"image/png","cols":196,"rows":71}
        ]}}});
        let mut missing = 0;
        let mut budget = 4096;
        assert!(
            prepare_history_entry(
                &mut entry,
                temp.path(),
                &mut budget,
                &mut missing,
                &CancellationToken::new()
            )
            .unwrap()
        );
        assert_eq!(
            entry["preserveData"]["snapcompact"]["frames"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            entry["preserveData"]["snapcompact"]["frames"][0]["data"],
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        assert_eq!(
            entry["preserveData"]["snapcompact"]["frames"][0]["detail"],
            "original"
        );
        assert!(
            entry["summary"]
                .as_str()
                .unwrap()
                .contains("Historical archive image unavailable")
        );
        assert!(
            !serde_json::to_string(&entry)
                .unwrap()
                .contains("blob:sha256:")
        );
    }

    #[test]
    fn historical_attachments_bound_markers_and_cover_provider_output_arrays() {
        let temp = TempDir::new().unwrap();
        let mut block = serde_json::json!({"type":"image"});
        let mut missing = 0;
        let mut budget = 1;
        let original = block.clone();
        assert!(
            prepare_history_block(
                &mut block,
                temp.path(),
                &mut budget,
                &mut missing,
                &CancellationToken::new()
            )
            .is_err()
        );
        assert_eq!(block, original);
        let mut record = serde_json::json!({"type":"message","message":{"role":"user","content":[],"providerPayload":{"type":"openaiResponsesHistory","items":[{"type":"function_call_output","call_id":"call1","output":[{"type":"input_image","image_url":"file:///missing.png"}]}]}}});
        let mut budget = 1024;
        assert!(
            prepare_history_entry(
                &mut record,
                temp.path(),
                &mut budget,
                &mut missing,
                &CancellationToken::new()
            )
            .unwrap()
        );
        assert_eq!(
            record["message"]["providerPayload"]["items"][0]["output"][0]["type"],
            "input_text"
        );
        assert!(budget < 1024);
        let mut source = serde_json::json!({"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"aGVsbG8="}});
        let original = source.clone();
        assert!(
            !prepare_history_block(
                &mut source,
                temp.path(),
                &mut budget,
                &mut missing,
                &CancellationToken::new()
            )
            .unwrap()
        );
        assert_eq!(source, original);
    }

    fn session_bytes(id: &str, cwd: &str) -> Vec<u8> {
        format!(
            "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n"
        )
        .into_bytes()
    }
    #[test]
    fn historical_attachments_resolve_verified_blobs_but_reject_corruption() {
        let temp = TempDir::new().unwrap();
        let bytes = b"attachment bytes";
        let hash = hex_sha256(bytes);
        std::fs::write(temp.path().join(&hash), bytes).unwrap();
        let original = serde_json::json!({"type":"image", "mimeType":"image/png", "data":format!("blob:sha256:{hash}")});
        let mut image = original.clone();
        let mut budget = 4096;
        let mut missing = 0;
        assert!(
            prepare_history_block(
                &mut image,
                temp.path(),
                &mut budget,
                &mut missing,
                &CancellationToken::new()
            )
            .unwrap()
        );
        assert_eq!(
            image["data"],
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        std::fs::write(temp.path().join(&hash), b"corrupt").unwrap();
        let mut image = original;
        assert!(
            prepare_history_block(
                &mut image,
                temp.path(),
                &mut budget,
                &mut missing,
                &CancellationToken::new()
            )
            .unwrap()
        );
        assert_eq!(image["type"], "text");
        assert_eq!(missing, 1);
    }

    #[test]
    fn historical_attachments_fail_open_without_changing_source_or_capture_contract() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        let mut bytes = session_bytes("omp-1", "/workspace");
        let record = serde_json::json!({"type":"message","message":{"role":"user","content":[
            {"type":"text","text":"file:///tmp/input.pdf"},
            {"type":"image","mimeType":"image/png","data":"blob:sha256:missing"},
            {"type":"image","mimeType":"image/png","data":"aGVsbG8="}
        ]}});
        bytes.extend(serde_json::to_vec(&record).unwrap());
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).unwrap();
        let token = CancellationToken::new();
        let captured = capture_omp_session_file(
            &path,
            Path::new("repo/session.jsonl"),
            "omp-1",
            "/workspace",
            &token,
        )
        .unwrap();
        let prepared = captured
            .prepare_historical_attachments(temp.path(), &token)
            .unwrap();
        let mut output = Vec::new();
        prepared.reopen().unwrap().read_to_end(&mut output).unwrap();
        let record: Value =
            serde_json::from_slice(output.split(|byte| *byte == b'\n').nth(2).unwrap()).unwrap();
        assert_eq!(record["message"]["content"][1]["type"], "text");
        assert!(
            record["message"]["content"][1]["text"]
                .as_str()
                .unwrap()
                .contains("Historical attachment unavailable")
        );
        assert_eq!(
            record["message"]["content"][0]["text"],
            "file:///tmp/input.pdf"
        );
        assert_eq!(record["message"]["content"][2]["data"], "aGVsbG8=");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(prepared.sha256, hex_sha256(&output));
        assert_eq!(prepared.byte_count, output.len() as u64);
        // Ordinary capture/current input is never normalized. Only the explicit
        // prior-history preparation step above can replace an attachment.
        let direct = capture_omp_session_artifact(
            &path,
            Path::new("repo/session.jsonl"),
            "omp-1",
            "/workspace",
        )
        .unwrap();
        assert_eq!(direct.bytes, bytes);
    }

    #[test]
    fn historical_attachments_preserve_tool_arguments_and_disclose_missing_native_files() {
        let arguments = serde_json::json!({"type":"image","data":"user-controlled argument"});
        let mut record = serde_json::json!({"type":"message","message":{"role":"assistant","content":[
            {"type":"toolCall","arguments":arguments}
        ],"providerPayload":{"type":"openaiResponsesHistory","items":[{"type":"message","role":"user","content":[
            {"type":"input_image","image_url":"data:image/png;base64,blob:sha256:missing"},
            {"type":"input_file","file_url":"file:///tmp/missing.pdf"},
            {"type":"input_audio","input_audio":{"data":"attachment://missing","format":"wav"}}
        ]}]}}});
        let mut missing = 0;
        let temp = TempDir::new().unwrap();
        let mut budget = 4096;
        assert!(
            prepare_history_entry(
                &mut record,
                temp.path(),
                &mut budget,
                &mut missing,
                &CancellationToken::new()
            )
            .unwrap()
        );
        assert_eq!(missing, 3);
        assert_eq!(record["message"]["content"][0]["arguments"], arguments);
        for block in record["message"]["providerPayload"]["items"][0]["content"]
            .as_array()
            .unwrap()
        {
            assert_eq!(block["type"], "input_text");
        }
        assert!(
            !prepare_history_entry(
                &mut record,
                temp.path(),
                &mut budget,
                &mut missing,
                &CancellationToken::new()
            )
            .unwrap()
        );
        assert!(valid_attachment_base64(
            &base64::engine::general_purpose::STANDARD.encode(vec![1; 2048])
        ));
        assert!(!valid_attachment_base64("YQ==YQ=="));
    }

    #[test]
    fn historical_attachments_bound_expansion_and_preserve_nonmessage_metadata() {
        let temp = TempDir::new().unwrap();
        let bytes = vec![1_u8; 768];
        let hash = hex_sha256(&bytes);
        std::fs::write(temp.path().join(&hash), &bytes).unwrap();
        let image = serde_json::json!({"type":"image","data":format!("blob:sha256:{hash}"),"mimeType":"image/png"});
        let mut record = serde_json::json!({"type":"message","message":{"role":"user","content":[image,image,image]}});
        let mut budget = 1200;
        let mut missing = 0;
        assert!(
            prepare_history_entry(
                &mut record,
                temp.path(),
                &mut budget,
                &mut missing,
                &CancellationToken::new()
            )
            .unwrap()
        );
        assert_eq!(missing, 2);
        assert_eq!(
            record["message"]["content"][0]["data"],
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        let mut metadata =
            serde_json::json!({"type":"custom","message":{"role":"user","content":[image]}});
        let original = metadata.clone();
        assert!(
            !prepare_history_entry(
                &mut metadata,
                temp.path(),
                &mut budget,
                &mut missing,
                &CancellationToken::new()
            )
            .unwrap()
        );
        assert_eq!(metadata, original);
    }

    #[test]
    #[cfg(unix)]
    fn historical_attachments_reject_fifo_and_symlink_without_blocking() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let hash = "a".repeat(64);
        let path = temp.path().join(&hash);
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read_historical_blob(temp.path(), &hash, 4096).is_none());
        std::fs::remove_file(&path).unwrap();
        symlink("/dev/zero", &path).unwrap();
        assert!(read_historical_blob(temp.path(), &hash, 4096).is_none());
    }

    #[test]
    fn captures_exact_bytes_digest_and_trusted_header() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        let bytes = session_bytes("omp-1", "/workspace");
        std::fs::write(&path, &bytes).unwrap();

        let artifact = capture_omp_session_artifact(
            &path,
            Path::new("repo/session.jsonl"),
            "omp-1",
            "/workspace",
        )
        .unwrap();
        assert_eq!(artifact.bytes, bytes);
        assert_eq!(artifact.byte_count, artifact.bytes.len() as u64);
        assert_eq!(artifact.native_session_id, "omp-1");
        assert_eq!(artifact.cwd, "/workspace");
        assert_eq!(artifact.storage_relative_path, "repo/session.jsonl");
        assert_eq!(artifact.sha256, hex_sha256(&artifact.bytes));
    }

    #[test]
    fn file_backed_capture_accepts_exact_limit_rejects_limit_plus_one_and_cancels() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        let mut source = File::create(&path).unwrap();
        source
            .write_all(&session_bytes("omp-1", "/workspace"))
            .unwrap();
        source.set_len(MAX_OMP_SESSION_ARTIFACT_BYTES).unwrap();
        drop(source);

        let captured = capture_omp_session_file(
            &path,
            Path::new("repo/session.jsonl"),
            "omp-1",
            "/workspace",
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(captured.byte_count, MAX_OMP_SESSION_ARTIFACT_BYTES);
        assert_eq!(
            captured.reopen().unwrap().metadata().unwrap().len(),
            MAX_OMP_SESSION_ARTIFACT_BYTES
        );
        drop(captured);

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            capture_omp_session_file(
                &path,
                Path::new("repo/session.jsonl"),
                "omp-1",
                "/workspace",
                &cancelled,
            )
            .is_err()
        );

        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_OMP_SESSION_ARTIFACT_BYTES + 1)
            .unwrap();
        assert!(
            capture_omp_session_file(
                &path,
                Path::new("repo/session.jsonl"),
                "omp-1",
                "/workspace",
                &CancellationToken::new(),
            )
            .is_err()
        );
    }

    #[test]
    fn file_backed_capture_ignores_a_truncated_record_after_the_session_header() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        let mut source = File::create(&path).unwrap();
        source
            .write_all(b"{\"type\":\"title\",\"title\":\"before\"}\n")
            .unwrap();
        source
            .write_all(b"{\"type\":\"session\",\"id\":\"omp-1\",\"cwd\":\"/workspace\"}\n")
            .unwrap();
        source
            .write_all(&vec![b'x'; MAX_OMP_SESSION_HEADER_BYTES])
            .unwrap();
        drop(source);

        let artifact = capture_omp_session_file(
            &path,
            Path::new("repo/session.jsonl"),
            "omp-1",
            "/workspace",
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(artifact.byte_count > MAX_OMP_SESSION_HEADER_BYTES as u64);
    }

    #[test]
    fn file_backed_capture_rejects_session_headers_beyond_the_identity_window() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        let mut source = File::create(&path).unwrap();
        source
            .write_all(&vec![b'x'; MAX_OMP_SESSION_HEADER_BYTES])
            .unwrap();
        source.write_all(b"\n").unwrap();
        source
            .write_all(b"{\"type\":\"session\",\"id\":\"omp-1\",\"cwd\":\"/workspace\"}\n")
            .unwrap();
        drop(source);
        assert!(
            capture_omp_session_file(
                &path,
                Path::new("repo/session.jsonl"),
                "omp-1",
                "/workspace",
                &CancellationToken::new(),
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_final_symlink() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target.jsonl");
        let link = temp.path().join("link.jsonl");
        std::fs::write(&target, session_bytes("omp-1", "/workspace")).unwrap();
        symlink(&target, &link).unwrap();
        assert!(
            capture_omp_session_artifact(
                &link,
                Path::new("repo/session.jsonl"),
                "omp-1",
                "/workspace"
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_multiply_linked_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        let alias = temp.path().join("alias.jsonl");
        std::fs::write(&path, session_bytes("omp-1", "/workspace")).unwrap();
        std::fs::hard_link(&path, alias).unwrap();
        assert!(
            capture_omp_session_artifact(
                &path,
                Path::new("repo/session.jsonl"),
                "omp-1",
                "/workspace"
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_directory_and_oversize_file() {
        let temp = TempDir::new().unwrap();
        assert!(
            capture_omp_session_artifact(
                temp.path(),
                Path::new("repo/session.jsonl"),
                "omp-1",
                "/workspace"
            )
            .is_err()
        );
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, session_bytes("omp-1", "/workspace")).unwrap();
        assert!(
            capture_with_limit(
                &path,
                Path::new("repo/session.jsonl"),
                "omp-1",
                "/workspace",
                5
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_header_identity_or_cwd_mismatch_and_malformed_jsonl() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, session_bytes("omp-real", "/real")).unwrap();
        assert!(
            capture_omp_session_artifact(
                &path,
                Path::new("repo/session.jsonl"),
                "omp-other",
                "/real"
            )
            .is_err()
        );
        assert!(
            capture_omp_session_artifact(
                &path,
                Path::new("repo/session.jsonl"),
                "omp-real",
                "/other"
            )
            .is_err()
        );
        std::fs::write(&path, b"not-json\n").unwrap();
        assert!(
            capture_omp_session_artifact(
                &path,
                Path::new("repo/session.jsonl"),
                "omp-real",
                "/real"
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_unsafe_storage_relative_paths() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, session_bytes("omp-1", "/workspace")).unwrap();
        for relative in [
            Path::new("../session.jsonl"),
            Path::new("repo/../../session.jsonl"),
            Path::new("/absolute/session.jsonl"),
            Path::new("repo/not-json.txt"),
        ] {
            assert!(
                capture_omp_session_artifact(&path, relative, "omp-1", "/workspace").is_err(),
                "accepted unsafe path: {}",
                relative.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn detects_same_length_rewrite_during_capture() {
        // Exercise the stable-file state comparison deterministically: the open
        // handle retains identity, while timestamps expose an in-place rewrite.
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        let original = session_bytes("omp-1", "/workspace");
        std::fs::write(&path, &original).unwrap();
        let file = File::open(&path).unwrap();
        let before = file.metadata().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let mut writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        writer.seek(SeekFrom::Start(0)).unwrap();
        let mut replacement = original.clone();
        *replacement.last_mut().unwrap() = b' ';
        writer.write_all(&replacement).unwrap();
        writer.sync_all().unwrap();
        let after = file.metadata().unwrap();
        assert!(!same_file_state(&before, &after));
    }
}
