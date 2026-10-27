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

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

use crate::EngineError;

/// Maximum native OMP session accepted for handoff (64 MiB).
pub const MAX_OMP_SESSION_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

const MAX_OMP_SESSION_HEADER_BYTES: usize = 64 * 1024;
// Local journals have no aggregate handoff limit; bound one parsed/expanded
// record instead, matching the native fork parser's 64 MiB record ceiling.
const MAX_LOCAL_OMP_RECORD_BYTES: u64 = 64 * 1024 * 1024;

/// Resolve a durable native identity within the configured OMP store only.
/// Capture an inactive journal; native handoff supplies a completed-turn fork
/// so the invoking runtime and its independent work can remain alive.
pub(crate) fn capture_omp_file_for_session(
    native_session_id: &str,
    cwd: &str,
    cancellation: &CancellationToken,
) -> Result<CapturedOmpSessionFile, EngineError> {
    let direct = Path::new(native_session_id);
    if direct.is_absolute() {
        if zeron_harness::omp::session_writer_state(direct) != zeron_harness::omp::SessionWriterState::Inactive {
            return Err(invalid("private native branch has an active or unknown writer"));
        }
        let mut header = Vec::new();
        open_regular_nofollow(direct)?.take(MAX_OMP_SESSION_HEADER_BYTES as u64).read_to_end(&mut header)?;
        let (id, _) = parse_session_header(&header)?;
        let relative = Path::new("crew-native").join(format!("{id}.jsonl"));
        return capture_omp_session_file(direct, &relative, &id, cwd, cancellation);
    }
    let root = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| {
            std::path::PathBuf::from(home).join(".omp/agent")
        }))
        .ok_or_else(|| invalid("OMP session root is unavailable"))?;
    capture_from_root(&root, native_session_id, cwd, cancellation)
}

/// The inventoried archive is immutable. OMP appends only to this durable,
/// self-contained launch copy; restarting must never reset it to the archive.
pub(crate) fn prepare_imported_native_session(
    archive: &Path, target: &Path, native_id: &str,
) -> Result<(), EngineError> {
    let header = |path: &Path| -> Result<(String, String), EngineError> {
        let file = open_regular_nofollow(path)?;
        validate_metadata(&file.metadata()?, u64::MAX)?;
        let mut bytes = Vec::new();
        file.take(MAX_OMP_SESSION_HEADER_BYTES as u64).read_to_end(&mut bytes)?;
        parse_session_header(&bytes)
    };
    let (id, cwd) = header(archive)?;
    if id != native_id { return Err(invalid("Imported native archive identity changed")); }
    match std::fs::symlink_metadata(target) {
        Ok(_) => {
            if header(target)? != (id, cwd) {
                return Err(invalid("Imported native launch identity or worktree changed"));
            }
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let cancellation = CancellationToken::new();
    let blobs = zeron_harness::omp::native_session_blob_dir(&archive.to_string_lossy(), false)
        .map_err(|error| EngineError::Other(error.to_string()))?;
    let captured = capture_omp_session_file_with_limit(
        archive, Path::new("session.jsonl"), &id, &cwd, &cancellation, u64::MAX,
    )?.prepare_historical_attachments_with_limits(
        &blobs, &cancellation, u64::MAX, MAX_LOCAL_OMP_RECORD_BYTES,
    )?;
    let parent = target.parent().ok_or_else(|| invalid("Native launch copy has no parent"))?;
    let mut output = NamedTempFile::new_in(parent)?;
    std::io::copy(&mut captured.reopen()?, &mut output)?;
    output.as_file().sync_all()?;
    output.persist_noclobber(target).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Make the copied branch self-contained before its durable preparation receipt.
pub(crate) fn prepare_native_branch_attachments(
    path: &Path, native_id: &str, cwd: &str, blob_dir: &Path,
) -> Result<(), EngineError> {
    let cancellation = CancellationToken::new();
    let captured = capture_omp_file_for_session(&path.to_string_lossy(), cwd, &cancellation)?;
    if captured.native_session_id != native_id { return Err(invalid("Private native branch identity changed")); }
    let captured = captured.prepare_historical_attachments(blob_dir, &cancellation)?;
    let parent = path.parent().ok_or_else(|| invalid("Private native branch has no parent"))?;
    let mut output = NamedTempFile::new_in(parent)?;
    std::io::copy(&mut captured.reopen()?, &mut output)?;
    output.as_file().sync_all()?;
    output.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn capture_from_root(
    root: &Path,
    native_session_id: &str,
    cwd: &str,
    cancellation: &CancellationToken,
) -> Result<CapturedOmpSessionFile, EngineError> {
    let sessions = root.join("sessions");
    let mut directories = vec![sessions.clone()];
    for entry in std::fs::read_dir(&sessions)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            directories.push(entry.path());
        }
    }
    let mut matched = None;
    for directory in directories {
        for entry in std::fs::read_dir(directory)? {
            check_cancelled(cancellation)?;
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file()
                || path.extension().and_then(|value| value.to_str()) != Some("jsonl")
            {
                continue;
            }
            let mut prefix = Vec::new();
            open_regular_nofollow(&path)?.take(MAX_OMP_SESSION_HEADER_BYTES as u64)
                .read_to_end(&mut prefix)?;
            let Ok((id, session_cwd)) = parse_session_header(&prefix) else { continue };
            if id != native_session_id || session_cwd != cwd { continue; }
            if matched.is_some() {
                return Err(invalid("multiple native OMP files have the same identity"));
            }
            matched = Some(path);
        }
    }
    let path = matched.ok_or_else(|| invalid("local OMP session is no longer available"))?;
    if !matches!(zeron_harness::omp::session_writer_state(&path),
        zeron_harness::omp::SessionWriterState::Inactive)
    {
        return Err(invalid("native OMP writer must be stopped before handoff"));
    }
    let relative = path.strip_prefix(&sessions)
        .map_err(|_| invalid("OMP session escaped the configured root"))?;
    capture_omp_session_file(&path, relative, native_session_id, cwd, cancellation)?
        .prepare_historical_attachments(&root.join("blobs"), cancellation)
}

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
        self,
        blob_dir: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Self, EngineError> {
        self.prepare_historical_attachments_with_limits(
            blob_dir, cancellation, MAX_OMP_SESSION_ARTIFACT_BYTES, MAX_OMP_SESSION_ARTIFACT_BYTES,
        )
    }

    fn prepare_historical_attachments_with_limits(
        mut self, blob_dir: &Path, cancellation: &CancellationToken,
        total_limit: u64, record_limit: u64,
    ) -> Result<Self, EngineError> {
        use std::io::{BufRead, BufReader};
        let mut output = NamedTempFile::new()?;
        let mut digest = Sha256::new();
        let mut byte_count = 0_u64;
        let mut missing = 0_usize;
        let mut total_budget = total_limit.saturating_sub(self.byte_count);
        let mut reader = BufReader::new(self.reopen()?);
        let mut line = Vec::new();
        loop {
            check_cancelled(cancellation)?;
            line.clear();
            let read = reader.by_ref().take(record_limit.saturating_add(1)).read_until(b'\n', &mut line)?;
            if read == 0 { break; }
            if line.len() as u64 > record_limit {
                return Err(invalid("OMP historical journal record exceeds the hydration limit"));
            }
            let newline = line.last() == Some(&b'\n');
            let body = if newline { &line[..line.len() - 1] } else { &line[..] };
            if body.is_empty() {
                output.write_all(&line)?;
                digest.update(&line);
                byte_count += line.len() as u64;
                continue;
            }
            let record_budget = record_limit.saturating_sub(line.len() as u64).min(total_budget);
            let mut expansion_budget = record_budget;
            let mut entry: Value = serde_json::from_slice(body)
                .map_err(|_| invalid("OMP historical session contains invalid JSON"))?;
            let mut changed = prepare_history_entry(
                &mut entry,
                blob_dir,
                &mut expansion_budget,
                &mut missing,
                cancellation,
            )?;
            changed |= prepare_native_image_fields(&mut entry, "", blob_dir, &mut expansion_budget, cancellation)?;
            total_budget = total_budget.saturating_sub(record_budget.saturating_sub(expansion_budget));
            let encoded;
            let bytes = if changed {
                encoded = serde_json::to_vec(&entry)
                    .map_err(|_| invalid("Could not encode prepared OMP history"))?;
                encoded.as_slice()
            } else { body };
            let record_bytes = bytes.len() as u64 + u64::from(newline);
            byte_count = byte_count.checked_add(record_bytes)
                .ok_or_else(|| invalid("Prepared OMP history size overflow"))?;
            if record_bytes > record_limit || byte_count > total_limit {
                return Err(invalid("Prepared OMP history exceeds capture limit"));
            }
            output.write_all(bytes)?;
            digest.update(bytes);
            if newline {
                output.write_all(b"\n")?;
                digest.update(b"\n");
            }
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
    capture_omp_session_file_with_limit(path, storage_relative_path,
        expected_native_session_id, expected_cwd, cancellation, MAX_OMP_SESSION_ARTIFACT_BYTES)
}

fn capture_omp_session_file_with_limit(
    path: &Path, storage_relative_path: &Path, expected_native_session_id: &str,
    expected_cwd: &str, cancellation: &CancellationToken, total_limit: u64,
) -> Result<CapturedOmpSessionFile, EngineError> {
    if expected_native_session_id.trim().is_empty() || expected_cwd.trim().is_empty() {
        return Err(invalid("OMP native session id and cwd are required"));
    }
    let storage_relative_path = validate_storage_relative_path(storage_relative_path)?;
    check_cancelled(cancellation)?;

    let mut input = open_regular_nofollow(path)?;
    let before = input.metadata()?;
    validate_metadata(&before, total_limit)?;
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
        if byte_count > total_limit || byte_count > before.len() {
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
    validate_metadata(&after, total_limit)?;
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

/// Match OMP session-loader's additional persisted image positions, including
/// tool images and provider image-generation results (not arbitrary text refs).
fn prepare_native_image_fields(
    value: &mut Value, key: &str, blobs: &Path, budget: &mut u64,
    cancellation: &CancellationToken,
) -> Result<bool, EngineError> {
    check_cancelled(cancellation)?;
    let mut changed = false;
    match value {
        Value::Array(items) => for item in items {
            changed |= prepare_native_image_fields(item, key, blobs, budget, cancellation)?;
        },
        Value::Object(fields) => {
            let image = key == "images" || (key == "content" && fields.get("type").and_then(Value::as_str) == Some("image"));
            let generated = fields.get("type").and_then(Value::as_str) == Some("image_generation_call");
            for (field, value) in fields {
                let native_data = (image && field == "data") || (generated && field == "result");
                if (native_data || field == "image_url") && value.as_str().is_some_and(|data| data.starts_with("blob:sha256:")) {
                    let hash = value.as_str().unwrap().strip_prefix("blob:sha256:").unwrap();
                    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
                        return Err(invalid("Malformed native historical image reference"));
                    }
                    let bytes = read_historical_blob(blobs, hash, *budget)
                        .ok_or_else(|| invalid("Native historical image blob missing, corrupt, or exceeds capture limit"))?;
                    let data = if native_data {
                        base64::engine::general_purpose::STANDARD.encode(bytes)
                    } else {
                        let data = String::from_utf8(bytes).map_err(|_| invalid("Native image URL is not UTF-8"))?;
                        if !data.strip_prefix("data:image/").and_then(|rest| rest.split_once(";base64,"))
                            .is_some_and(|(_, data)| valid_attachment_base64(data)) {
                            return Err(invalid("Native image URL blob is invalid"));
                        }
                        data
                    };
                    *budget = budget.saturating_sub(data.len() as u64 + 128);
                    *value = Value::String(data);
                    changed = true;
                } else {
                    changed |= prepare_native_image_fields(value, field, blobs, budget, cancellation)?;
                }
            }
        }
        _ => {}
    }
    Ok(changed)
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
                        // OMP externalizeImageDataUrl stores the entire UTF-8
                        // data URL, unlike image.data blobs (raw binary).
                        let replacement = if url && encoded.is_none() {
                            let data_url = String::from_utf8(bytes)
                                .map_err(|_| invalid("Native provider image URL blob is not UTF-8"))?;
                            if !data_url.strip_prefix("data:image/")
                                .and_then(|rest| rest.split_once(";base64,"))
                                .is_some_and(|(_, data)| valid_attachment_base64(data)) {
                                return Err(invalid("Native provider image URL blob is invalid"));
                            }
                            *budget = budget.saturating_sub(data_url.len() as u64 + 128);
                            data_url
                        } else {
                            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                            *budget = budget.saturating_sub(encoded.len() as u64 + 128);
                            if url {
                                let media_type = data.strip_prefix("data:")
                                    .and_then(|rest| rest.split_once(';'))
                                    .map(|(mime, _)| mime).unwrap_or("image/png");
                                format!("data:{media_type};base64,{encoded}")
                            } else { encoded }
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
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
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
    fn local_import_streams_past_total_limit_but_bounds_each_record() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("session.jsonl");
        let mut bytes = session_bytes("native", "/repo");
        for _ in 0..16 { bytes.extend_from_slice(b"{\"type\":\"custom\",\"data\":\"retained\"}\n"); }
        bytes.extend_from_slice(b"\n");
        std::fs::write(&source, &bytes).unwrap();
        let cancellation = CancellationToken::new();
        let capture = |limit| capture_omp_session_file_with_limit(
            &source, Path::new("session.jsonl"), "native", "/repo", &cancellation, limit,
        );
        // Small parameters exercise the same aggregate-vs-record boundary
        // without allocating a 64 MiB fixture in the normal test suite.
        assert!(capture(256).is_err());
        let captured = capture(u64::MAX).unwrap();
        assert_eq!(captured.sha256, hex_sha256(&bytes));
        let prepared = captured.prepare_historical_attachments_with_limits(
            temp.path(), &cancellation, u64::MAX, 128,
        ).unwrap();
        assert_eq!(prepared.byte_count, bytes.len() as u64);
        assert_eq!(prepared.sha256, hex_sha256(&bytes));
        assert!(capture(u64::MAX).unwrap().prepare_historical_attachments_with_limits(
            temp.path(), &cancellation, 256, 128,
        ).is_err());
        let target = temp.path().join("launch.jsonl");
        prepare_imported_native_session(&source, &target, "native").unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        let oversized = format!("{{\"type\":\"custom\",\"data\":\"{}\"}}\n", "x".repeat(128));
        std::fs::OpenOptions::new().append(true).open(&source).unwrap()
            .write_all(oversized.as_bytes()).unwrap();
        assert!(capture(u64::MAX).unwrap().prepare_historical_attachments_with_limits(
            temp.path(), &cancellation, u64::MAX, 128,
        ).is_err());
        // The public remote handoff entry point retains its exact 64 MiB cap.
        let remote = temp.path().join("remote.jsonl");
        File::create(&remote).unwrap().set_len(MAX_OMP_SESSION_ARTIFACT_BYTES + 1).unwrap();
        assert!(capture_omp_session_file(&remote, Path::new("remote.jsonl"),
            "native", "/repo", &cancellation).is_err());
    }

    /// Supply only a stopped-writer throwaway archive copy with sibling blobs,
    /// never the live source journal. This checks the actual oversized history;
    /// real inference/identity continuity remains the separate native OMP smoke.
    #[test]
    #[ignore = "actual >64 MiB imported journal copy via COMET_TEST_LARGE_OMP_ARCHIVE"]
    fn local_import_large_archive_copy() {
        fn digest(path: &Path) -> (u64, String) {
            let mut input = File::open(path).unwrap();
            let mut digest = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            let mut size = 0;
            loop {
                let read = input.read(&mut buffer).unwrap();
                if read == 0 { break; }
                size += read as u64;
                digest.update(&buffer[..read]);
            }
            (size, format!("{:x}", digest.finalize()))
        }
        let archive = std::path::PathBuf::from(std::env::var_os("COMET_TEST_LARGE_OMP_ARCHIVE")
            .expect("stopped-writer throwaway archive copy required"));
        let before = digest(&archive);
        assert!(before.0 > MAX_OMP_SESSION_ARTIFACT_BYTES);
        let mut header = Vec::new();
        File::open(&archive).unwrap().take(MAX_OMP_SESSION_HEADER_BYTES as u64)
            .read_to_end(&mut header).unwrap();
        let (id, cwd) = parse_session_header(&header).unwrap();
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("launch.jsonl");
        prepare_imported_native_session(&archive, &target, &id).unwrap();
        let launch = digest(&target);
        assert!(launch.0 > MAX_OMP_SESSION_ARTIFACT_BYTES);
        header.clear();
        File::open(&target).unwrap().take(MAX_OMP_SESSION_HEADER_BYTES as u64)
            .read_to_end(&mut header).unwrap();
        assert_eq!(parse_session_header(&header).unwrap(), (id.clone(), cwd));
        prepare_imported_native_session(&archive, &target, &id).unwrap();
        assert_eq!(digest(&target), launch);
        assert_eq!(digest(&archive), before);
    }

    #[test]
    fn imported_native_blobs_keep_archive_and_continued_launch_journal() {
        let temp = TempDir::new().unwrap();
        let store = temp.path().join("native-histories/0");
        std::fs::create_dir_all(store.join("sessions/project")).unwrap();
        std::fs::create_dir(store.join("blobs")).unwrap();
        let archive = store.join("sessions/project/original.jsonl");
        let bytes = b"exact binary image bytes";
        let hash = hex_sha256(bytes);
        std::fs::write(store.join("blobs").join(&hash), bytes).unwrap();
        let data_url = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes));
        let url_hash = hex_sha256(data_url.as_bytes());
        std::fs::write(store.join("blobs").join(&url_hash), &data_url).unwrap();
        let mut raw = session_bytes("exact-native", "/original/repo");
        let record = serde_json::json!({"type":"message","message":{"role":"user","content":[
            {"type":"image","mimeType":"image/png","data":format!("blob:sha256:{hash}")}
        ],"providerPayload":{"type":"openaiResponsesHistory","items":[
            {"type":"message","role":"user","content":[{"type":"input_image","image_url":format!("blob:sha256:{url_hash}")}]},
            {"type":"image_generation_call","result":format!("blob:sha256:{hash}")}
        ]},"images":[{"data":format!("blob:sha256:{hash}"),"mimeType":"image/png"}]}});
        raw.extend(serde_json::to_vec(&record).unwrap());
        raw.push(b'\n');
        std::fs::write(&archive, &raw).unwrap();
        let target = temp.path().join("launch.jsonl");
        prepare_imported_native_session(&archive, &target, "exact-native").unwrap();
        let hydrated = std::fs::read(&target).unwrap();
        let last: Value = serde_json::from_slice(hydrated.split(|b| *b == b'\n').filter(|line| !line.is_empty()).last().unwrap()).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert_eq!(last["message"]["content"][0]["data"], encoded);
        assert_eq!(last["message"]["images"][0]["data"], encoded);
        assert_eq!(last["message"]["providerPayload"]["items"][0]["content"][0]["image_url"], data_url);
        assert_eq!(last["message"]["providerPayload"]["items"][1]["result"], encoded);
        let mut continued = hydrated;
        continued.extend_from_slice(b"{\"type\":\"custom\",\"data\":\"native followup\"}\n");
        std::fs::write(&target, &continued).unwrap();
        prepare_imported_native_session(&archive, &target, "exact-native").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), continued);
        assert_eq!(std::fs::read(&archive).unwrap(), raw);
        std::fs::write(store.join("blobs").join(&hash), b"corrupt").unwrap();
        let rejected = temp.path().join("corrupt.jsonl");
        assert!(prepare_imported_native_session(&archive, &rejected, "exact-native").is_err());
        assert!(!rejected.exists());
        assert_eq!(std::fs::read(&archive).unwrap(), raw);
    }

    /// Run with an isolated browser-authenticated Crew profile, never provider credentials.
    /// The PNG must contain a visual nonce absent from all prompt/journal text.
    #[tokio::test]
    #[ignore = "real installed OMP + managed vision inference; needs isolated Crew auth root, HOME/PI_CODING_AGENT_DIR, executable, model and visual-nonce PNG"]
    async fn imported_native_blobs_resume_in_real_omp() {
        use futures::StreamExt;
        use zeron_harness::{Harness, OmpHarness, RunContext, RunControls};
        use zeron_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel};
        let executable = std::env::var_os("COMET_TEST_OMP_EXECUTABLE").expect("native OMP executable required");
        let model = std::env::var("COMET_TEST_OMP_MODEL").expect("authenticated vision model required");
        let png = std::fs::read(std::env::var_os("COMET_TEST_OMP_BLOB_PNG").expect("visual nonce PNG required")).unwrap();
        assert!(png.len() >= 1024, "use a PNG large enough for OMP's 1024-base64-character blob externalization threshold");
        let visual_nonce = std::env::var("COMET_TEST_OMP_VISUAL_NONCE").expect("expected visual nonce required");
        assert!(!visual_nonce.is_empty());
        assert!(std::env::var_os("PI_CODING_AGENT_DIR").is_some(), "use a disposable native store, never installed histories");
        assert!(std::env::var_os("OMP_PROFILE").is_none() && std::env::var_os("PI_PROFILE").is_none(), "profiles override isolated agent dir");
        let temp = TempDir::new().unwrap();
        assert!(crate::devbox::host_binding().unwrap().is_none(), "this local smoke must not bypass managed sandbox inference admission");
        let auth_root = std::path::PathBuf::from(std::env::var_os("COMET_TEST_CREW_AUTH_ROOT")
            .expect("normally signed-in isolated Crew auth root required; do not copy provider credentials"));
        let saved = zeron_sync::scaffold_auth::load_session(&auth_root.join("scaffold-session.json"))
            .unwrap().expect("complete normal browser login in the isolated Crew profile first");
        assert!(saved.device.is_none(), "local smoke requires user authority, not a sandbox grant");
        let mut authority = zeron_sync::scaffold_auth::ScaffoldConfig::new(
            saved.resource.clone(), saved.edge_origin.clone(), saved.identity.project_scope.clone());
        authority.deployment_id = saved.identity.deployment_id.clone();
        let mut config = crate::AuthConfig::new(&saved.edge_origin, &auth_root);
        config.scaffold = Some(authority.clone());
        drop(saved);
        let auth = crate::Auth::detect(config).await;
        assert!(auth.state().is_signed_in(), "isolated Crew authority must reverify successfully");
        let deployment = auth.verified_scope().and_then(|scope| scope.deployment_id)
            .expect("verified deployment required");
        let client = crate::scaffold::ScaffoldClient::new(&authority.control_origin,
            &authority.project_scope, auth.for_profile_tokens()).unwrap();
        let grants = crate::scaffold::EdgeDeviceJoinGrantClient::new(&authority.edge_origin,
            auth.for_profile_tokens()).unwrap();
        let runtime = crate::scaffold::ScaffoldRuntime::new(client, &authority.edge_origin,
            std::sync::Arc::new(grants)).with_deployment_id(deployment);
        let crew = crate::crew::CrewServices::new(auth, runtime,
            std::sync::Arc::new(zeron_sync::DocsStore::open(temp.path().join("managed-docs")).unwrap())).unwrap();
        let cwd = temp.path().canonicalize().unwrap().to_string_lossy().into_owned();
        let new_image = temp.path().join("new-native-image.png");
        std::fs::write(&new_image, &png).unwrap();
        let store = temp.path().join("native-histories/0");
        std::fs::create_dir_all(store.join("sessions/project")).unwrap();
        std::fs::create_dir(store.join("blobs")).unwrap();
        let hash = hex_sha256(&png);
        std::fs::write(store.join("blobs").join(&hash), &png).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let memory = uuid::Uuid::new_v4().to_string();
        let followup = uuid::Uuid::new_v4().to_string();
        let header = serde_json::json!({"type":"session","version":3,"id":id,"cwd":cwd,"timestamp":"2026-10-20T00:00:00.000Z"});
        let message = serde_json::json!({"type":"message","id":"abcd1234","parentId":null,"timestamp":"2026-10-20T00:00:01.000Z",
            "message":{"role":"user","timestamp":1792454401000_u64,"content":[
                {"type":"text","text":format!("Remember archive token {memory} and the text in this image.")},
                {"type":"image","mimeType":"image/png","data":format!("blob:sha256:{hash}")}
            ]}});
        let raw = format!("{header}\n{message}\n");
        let archive = store.join("sessions/project/original.jsonl");
        std::fs::write(&archive, &raw).unwrap();
        let launches = temp.path().join("native-resumes");
        std::fs::create_dir(&launches).unwrap();
        let target = launches.join(format!("{id}.jsonl"));
        let branch_dir = temp.path().join("native-branches");
        std::fs::create_dir(&branch_dir).unwrap();
        let branch_id = uuid::Uuid::new_v4().to_string();
        let branch = branch_dir.join(format!("{branch_id}.jsonl"));
        let harness = OmpHarness::new().with_executable(executable);
        for turn in 0..3 {
            prepare_imported_native_session(&archive, &target, &id).unwrap();
            let (journal, expected_id) = if turn == 2 {
                assert!(std::fs::read_to_string(&target).unwrap().contains("blob:sha256:"), "second native turn must persist a real blob reference");
                zeron_harness::omp::fork_native_session(target.to_str().unwrap(), &branch, &branch_id, false).unwrap();
                let blobs = zeron_harness::omp::native_session_blob_dir(target.to_str().unwrap(), false).unwrap();
                prepare_native_branch_attachments(&branch, &branch_id, &cwd, &blobs).unwrap();
                let prepared = std::fs::read_to_string(&branch).unwrap();
                assert!(!prepared.contains("blob:sha256:") && !prepared.contains("Historical attachment unavailable"), "fork must carry the native-written blob bytes, not a missing-attachment marker");
                (&branch, &branch_id)
            } else { (&target, &id) };
            let prompt = if turn == 0 {
                format!("Reply with the archive token and exact text visible in the earlier image. Also remember continuation token {followup}. Do not use tools.")
            } else { "Reply with the archive token, continuation token, and exact text visible in the original image. Do not use tools.".into() };
            let (_, steering) = tokio::sync::mpsc::channel(1);
            let route = crew.prepare_inference(expected_id, HarnessId::Omp, Some(&model), None, None)
                .await.expect("real Crew Agent Auth route issuance");
            let controls = RunControls { request_input: Box::new(|_| {
                let (tx, rx) = tokio::sync::oneshot::channel(); let _ = tx.send(Vec::new()); rx
            }), steering, interrupt: CancellationToken::new(),
                context: Some(RunContext { session_id: expected_id.clone(), ipc_port: 0,
                    supervision_token: None, inference: Some(route.clone()), fork_from: None }), realtime: None,
                execution_lease: None, turn: Default::default() };
            let request = RunRequest { prompt, model: Some(model.clone()), harness: None, worktree: None,
                mcp: None, reasoning: None, model_options: serde_json::Map::new(), cwd: cwd.clone(),
                sandbox: SandboxLevel::DangerFullAccess, auto_approve: false,
                resume: Some(journal.to_string_lossy().into_owned()),
                attachments: if turn == 1 { vec![new_image.to_string_lossy().into_owned()] } else { Vec::new() } };
            let mut events = harness.run(request, controls).await.expect("real native resume starts");
            let (mut text, mut identity, mut completed) = (String::new(), false, false);
            tokio::time::timeout(std::time::Duration::from_secs(180), async {
                while let Some(event) = events.next().await {
                    match event.expect("real native event") {
                        AgentEvent::SessionStarted { harness: HarnessId::Omp, session_id, .. } => {
                            assert_eq!(&session_id, expected_id); identity = true;
                        }
                        AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                        AgentEvent::Done { status, .. } => { assert_eq!(status, DoneStatus::Completed); completed = true; }
                        AgentEvent::Error { message } => panic!("native error: {message}"),
                        _ => {}
                    }
                }
            }).await.expect("real native turn completes");
            crew.retire_inference(&route);
            assert!(identity && completed);
            assert!(text.contains(&memory) && text.contains(&visual_nonce), "turn {turn}: {text}");
            if turn > 0 { assert!(text.contains(&followup), "native continuation lost: {text}"); }
            assert_eq!(std::fs::read_to_string(&archive).unwrap(), raw);
        }
    }

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
        let direct = capture_omp_session_file(
            &path,
            Path::new("repo/session.jsonl"),
            "omp-1",
            "/workspace",
            &token,
        )
        .unwrap();
        let mut original = Vec::new();
        direct.reopen().unwrap().read_to_end(&mut original).unwrap();
        assert_eq!(original, bytes);
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

        let artifact = capture_omp_session_file(
            &path,
            Path::new("repo/session.jsonl"),
            "omp-1",
            "/workspace",
            &CancellationToken::new(),
        )
        .unwrap();
        let mut captured = Vec::new();
        artifact.reopen().unwrap().read_to_end(&mut captured).unwrap();
        assert_eq!(captured, bytes);
        assert_eq!(artifact.byte_count, captured.len() as u64);
        assert_eq!(artifact.native_session_id, "omp-1");
        assert_eq!(artifact.cwd, "/workspace");
        assert_eq!(artifact.storage_relative_path, "repo/session.jsonl");
        assert_eq!(artifact.sha256, hex_sha256(&captured));
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
            capture_omp_session_file(
                &link,
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
    fn rejects_multiply_linked_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        let alias = temp.path().join("alias.jsonl");
        std::fs::write(&path, session_bytes("omp-1", "/workspace")).unwrap();
        std::fs::hard_link(&path, alias).unwrap();
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
    fn rejects_directory() {
        let temp = TempDir::new().unwrap();
        assert!(
            capture_omp_session_file(
                temp.path(),
                Path::new("repo/session.jsonl"),
                "omp-1",
                "/workspace",
                &CancellationToken::new(),
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
            capture_omp_session_file(
                &path,
                Path::new("repo/session.jsonl"),
                "omp-other",
                "/real",
                &CancellationToken::new(),
            )
            .is_err()
        );
        assert!(
            capture_omp_session_file(
                &path,
                Path::new("repo/session.jsonl"),
                "omp-real",
                "/other",
                &CancellationToken::new(),
            )
            .is_err()
        );
        std::fs::write(&path, b"not-json\n").unwrap();
        assert!(
            capture_omp_session_file(
                &path,
                Path::new("repo/session.jsonl"),
                "omp-real",
                "/real",
                &CancellationToken::new(),
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
                capture_omp_session_file(&path, relative, "omp-1", "/workspace", &CancellationToken::new()).is_err(),
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
