//! Trusted capture of an OMP native session file for exact platform handoff.
//!
//! The capture is intentionally local-controller only. It opens an already
//! discovered path without following a final symlink, rejects multiply-linked
//! files, bounds the read, and validates identity/metadata before and after the
//! read so bytes cannot silently change underneath the capture.

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

    fn session_bytes(id: &str, cwd: &str) -> Vec<u8> {
        format!(
            "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n"
        )
        .into_bytes()
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
