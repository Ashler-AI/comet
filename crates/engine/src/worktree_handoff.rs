//! Bounded, content-addressed capture of one Git worktree relative to the exact
//! commit Scaffold checked out.

use std::collections::BTreeSet;
use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

use crate::EngineError;

pub(crate) const MAX_HANDOFF_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_HANDOFF_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HANDOFF_FILES: usize = 25_000;
const MAX_TAR_LISTING_BYTES: usize = 768 * 1024;
const MAX_GIT_PATH_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_MANIFEST_VARIABLE_BYTES: usize = 8 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
const MANIFEST_PATH: &str = ".crew-handoff-manifest.json";

#[derive(Debug)]
pub(crate) struct WorktreeHandoffArchive {
    file: NamedTempFile,
    pub byte_count: u64,
    pub manifest_sha256: String,
    pub base_sha: String,
    pub entry_count: usize,
}

impl WorktreeHandoffArchive {
    pub fn reopen(&self) -> io::Result<File> {
        self.file.reopen()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    version: &'static str,
    base_sha: String,
    entries: Vec<ManifestEntry>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ManifestEntry {
    Regular {
        path: String,
        sha256: String,
        #[serde(rename = "byteCount")]
        byte_count: u64,
        executable: bool,
    },
    Symlink {
        path: String,
        target: String,
    },
    Delete {
        path: String,
    },
}

impl ManifestEntry {
    fn path(&self) -> &str {
        match self {
            Self::Regular { path, .. } | Self::Symlink { path, .. } | Self::Delete { path } => path,
        }
    }
}
#[cfg(test)]
pub(crate) fn capture_worktree_handoff(
    cwd: &Path,
    expected_base_sha: &str,
) -> Result<WorktreeHandoffArchive, EngineError> {
    capture_worktree_handoff_cancellable(cwd, expected_base_sha, &CancellationToken::new())
}

pub(crate) fn capture_worktree_handoff_cancellable(
    cwd: &Path,
    expected_base_sha: &str,
    cancellation: &CancellationToken,
) -> Result<WorktreeHandoffArchive, EngineError> {
    check_cancelled(cancellation)?;
    let expected_base_sha = validate_base_sha(expected_base_sha)?;
    let root_output = run_git_bounded(
        cwd,
        &["rev-parse", "--show-toplevel"],
        16 * 1024,
        cancellation,
    )?;
    let root_text = std::str::from_utf8(trim_ascii(&root_output))
        .map_err(|_| invalid("Git worktree root is not UTF-8"))?;
    let root = PathBuf::from(root_text);
    let canonical_root = root.canonicalize()?;
    let canonical_cwd = cwd.canonicalize()?;
    if canonical_cwd != canonical_root && !canonical_cwd.starts_with(&canonical_root) {
        return Err(invalid("OMP session cwd is outside its Git worktree"));
    }

    let verify_arg = format!("{expected_base_sha}^{{commit}}");
    let resolved = run_git_bounded(
        &canonical_root,
        &["rev-parse", "--verify", &verify_arg],
        1024,
        cancellation,
    )?;
    let resolved = std::str::from_utf8(trim_ascii(&resolved))
        .map_err(|_| invalid("Git base commit is not UTF-8"))?;
    if !resolved.eq_ignore_ascii_case(expected_base_sha) {
        return Err(invalid("Scaffold checkout commit is unavailable locally"));
    }

    let mut paths = BTreeSet::new();
    let mut listing_bytes = 0_usize;
    let changed = run_git_bounded(
        &canonical_root,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            expected_base_sha,
            "--",
        ],
        MAX_GIT_PATH_OUTPUT_BYTES,
        cancellation,
    )?;
    insert_git_paths(&mut paths, &mut listing_bytes, &changed)?;
    drop(changed);
    let untracked = run_git_bounded(
        &canonical_root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        MAX_GIT_PATH_OUTPUT_BYTES,
        cancellation,
    )?;
    insert_git_paths(&mut paths, &mut listing_bytes, &untracked)?;
    drop(untracked);
    let ignored_context = run_git_bounded(
        &canonical_root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
            "--",
            ".omx/specs",
            ".omx/interviews",
            ".omx/plans",
        ],
        MAX_GIT_PATH_OUTPUT_BYTES,
        cancellation,
    )?;
    insert_git_paths(&mut paths, &mut listing_bytes, &ignored_context)?;
    drop(ignored_context);
    if paths.len() > MAX_HANDOFF_FILES {
        return Err(invalid("Worktree handoff contains too many changed paths"));
    }

    let mut archive = NamedTempFile::new()?;
    let mut manifest_variable_bytes = listing_bytes;
    let mut entries = Vec::with_capacity(paths.len());
    for path in paths {
        check_cancelled(cancellation)?;
        let source = canonical_root.join(&path);
        match std::fs::symlink_metadata(&source) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = std::fs::read_link(&source)?;
                let target = validate_symlink_target(&canonical_root, &source, &target)?;
                add_manifest_variable_bytes(&mut manifest_variable_bytes, target.len())?;
                entries.push(ManifestEntry::Symlink { path, target });
            }
            Ok(metadata) if metadata.is_file() => {
                let executable = is_executable(&metadata);
                let archive_path = format!("files/{path}");
                let (sha256, byte_count) = append_regular_file(
                    archive.as_file_mut(),
                    &archive_path,
                    &source,
                    &metadata,
                    executable,
                    cancellation,
                )?;
                entries.push(ManifestEntry::Regular {
                    path,
                    sha256,
                    byte_count,
                    executable,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                entries.push(ManifestEntry::Delete { path });
            }
            Ok(_) => return Err(invalid("Worktree handoff supports only files and symlinks")),
            Err(error) => return Err(error.into()),
        }
        enforce_archive_limit(archive.as_file())?;
    }
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    let entry_count = entries.len();

    let manifest = serde_json::to_vec(&Manifest {
        version: "crew.scaffold.worktree.v1",
        base_sha: expected_base_sha.to_string(),
        entries,
    })
    .map_err(|error| invalid(&format!("Could not encode worktree manifest: {error}")))?;
    if manifest.len() > MAX_MANIFEST_BYTES {
        return Err(invalid("Worktree handoff manifest exceeds its limit"));
    }
    let manifest_sha256 = hex_sha256(&manifest);
    append_bytes(archive.as_file_mut(), MANIFEST_PATH, &manifest, 0o600)?;
    append_tar_end(archive.as_file_mut())?;
    archive.as_file_mut().flush()?;
    check_cancelled(cancellation)?;
    enforce_archive_limit(archive.as_file())?;

    let byte_count = archive.as_file().metadata()?.len();
    Ok(WorktreeHandoffArchive {
        file: archive,
        byte_count,
        manifest_sha256,
        base_sha: expected_base_sha.to_string(),
        entry_count,
    })
}

fn validate_base_sha(value: &str) -> Result<&str, EngineError> {
    let value = value.trim();
    if !(value.len() == 40 || value.len() == 64)
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid("Scaffold did not return an exact checkout commit"));
    }
    Ok(value)
}

fn insert_git_paths(
    paths: &mut BTreeSet<String>,
    listing_bytes: &mut usize,
    output: &[u8],
) -> Result<(), EngineError> {
    for raw in output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(raw)
            .map_err(|_| invalid("Git path is not UTF-8"))?
            .to_string();
        validate_repo_path(&path)?;
        let path_bytes = "files/"
            .len()
            .checked_add(path.len())
            .and_then(|bytes| bytes.checked_add(1))
            .ok_or_else(|| invalid("Worktree handoff path listing exceeds its limit"))?;
        if paths.insert(path) {
            *listing_bytes = listing_bytes
                .checked_add(path_bytes)
                .ok_or_else(|| invalid("Worktree handoff path listing exceeds its limit"))?;
            if *listing_bytes > MAX_TAR_LISTING_BYTES {
                return Err(invalid("Worktree handoff path listing exceeds its limit"));
            }
        }
        if paths.len() > MAX_HANDOFF_FILES {
            return Err(invalid("Worktree handoff contains too many changed paths"));
        }
    }
    Ok(())
}

fn add_manifest_variable_bytes(total: &mut usize, bytes: usize) -> Result<(), EngineError> {
    *total = total
        .checked_add(bytes)
        .ok_or_else(|| invalid("Worktree handoff manifest input exceeds its limit"))?;
    if *total > MAX_MANIFEST_VARIABLE_BYTES {
        return Err(invalid("Worktree handoff manifest input exceeds its limit"));
    }
    Ok(())
}

fn validate_repo_path(path: &str) -> Result<(), EngineError> {
    if path.is_empty()
        || path.len() > 4096
        || path.starts_with('/')
        || path.contains('\\')
        || path.bytes().any(|byte| byte < 32 || byte == 127)
    {
        return Err(invalid("Worktree handoff contains an unsafe path"));
    }
    let mut components = Path::new(path).components();
    let first = components.next();
    if !matches!(first, Some(Component::Normal(_)))
        || components.any(|component| !matches!(component, Component::Normal(_)))
        || matches!(path.split('/').next(), Some(".git" | ".scaffold"))
    {
        return Err(invalid("Worktree handoff contains an unsafe path"));
    }
    Ok(())
}

fn validate_symlink_target(
    root: &Path,
    source: &Path,
    target: &Path,
) -> Result<String, EngineError> {
    if target.is_absolute() {
        return Err(invalid("Worktree handoff rejects absolute symlinks"));
    }
    let target = target
        .to_str()
        .filter(|value| !value.is_empty() && !value.contains('\\'))
        .ok_or_else(|| invalid("Worktree handoff symlink target is not safe UTF-8"))?;
    let mut normalized = PathBuf::new();
    for component in source.parent().unwrap_or(root).join(target).components() {
        match component {
            Component::RootDir => normalized.push(Path::new("/")),
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(invalid("Worktree handoff symlink escapes the repository"));
                }
            }
            Component::CurDir => {}
            Component::Prefix(_) => {
                return Err(invalid("Worktree handoff symlink target is unsupported"));
            }
        }
    }
    if normalized != root && !normalized.starts_with(root) {
        return Err(invalid("Worktree handoff symlink escapes the repository"));
    }
    Ok(target.to_string())
}

fn append_regular_file(
    archive: &mut File,
    archive_path: &str,
    source: &Path,
    expected: &Metadata,
    executable: bool,
    cancellation: &CancellationToken,
) -> Result<(String, u64), EngineError> {
    if expected.len() > MAX_HANDOFF_FILE_BYTES {
        return Err(invalid("Worktree handoff file exceeds its limit"));
    }
    let mut input = open_regular_nofollow(source)?;
    let before = input.metadata()?;
    if !before.is_file() || !same_file_state(expected, &before) {
        return Err(invalid("Worktree file changed before capture"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if before.nlink() != 1 {
            return Err(invalid("Worktree handoff rejects hard-linked files"));
        }
    }
    append_tar_header(
        archive,
        archive_path,
        before.len(),
        if executable { 0o755 } else { 0o644 },
    )?;
    let mut digest = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_cancelled(cancellation)?;
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied = copied.saturating_add(read as u64);
        if copied > MAX_HANDOFF_FILE_BYTES || copied > before.len() {
            return Err(invalid("Worktree file changed during capture"));
        }
        digest.update(&buffer[..read]);
        archive.write_all(&buffer[..read])?;
    }
    if copied != before.len() {
        return Err(invalid("Worktree file changed during capture"));
    }
    let after = input.metadata()?;
    if !same_file_state(&before, &after) {
        return Err(invalid("Worktree file changed during capture"));
    }
    pad_tar_entry(archive, copied)?;
    Ok((format!("{:x}", digest.finalize()), copied))
}

fn append_bytes(
    archive: &mut File,
    path: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<(), EngineError> {
    append_tar_header(archive, path, bytes.len() as u64, mode)?;
    archive.write_all(bytes)?;
    pad_tar_entry(archive, bytes.len() as u64)?;
    Ok(())
}

fn append_tar_header(
    archive: &mut File,
    path: &str,
    byte_count: u64,
    mode: u32,
) -> Result<(), EngineError> {
    validate_repo_path(path).or_else(|_| {
        if path == MANIFEST_PATH || path.starts_with("files/") {
            Ok(())
        } else {
            Err(invalid("Worktree archive path is unsafe"))
        }
    })?;
    let (name, prefix) = split_ustar_path(path)?;
    let mut header = [0_u8; 512];
    put_tar_text(&mut header[0..100], name)?;
    put_tar_octal(&mut header[100..108], mode as u64)?;
    put_tar_octal(&mut header[108..116], 0)?;
    put_tar_octal(&mut header[116..124], 0)?;
    put_tar_octal(&mut header[124..136], byte_count)?;
    put_tar_octal(&mut header[136..148], 0)?;
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    put_tar_text(&mut header[345..500], prefix)?;
    let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    let checksum_field = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(checksum_field.as_bytes());
    archive.write_all(&header)?;
    Ok(())
}

fn split_ustar_path(path: &str) -> Result<(&str, &str), EngineError> {
    if path.len() <= 100 {
        return Ok((path, ""));
    }
    for (index, _) in path.match_indices('/').rev() {
        let (prefix, rest) = path.split_at(index);
        let name = &rest[1..];
        if prefix.len() <= 155 && !name.is_empty() && name.len() <= 100 {
            return Ok((name, prefix));
        }
    }
    Err(invalid("Worktree handoff path exceeds ustar limits"))
}

fn put_tar_text(field: &mut [u8], value: &str) -> Result<(), EngineError> {
    if value.as_bytes().contains(&0) || value.len() > field.len() {
        return Err(invalid("Worktree handoff path exceeds ustar limits"));
    }
    field[..value.len()].copy_from_slice(value.as_bytes());
    Ok(())
}

fn put_tar_octal(field: &mut [u8], value: u64) -> Result<(), EngineError> {
    let digits = format!("{value:o}");
    if digits.len() + 1 > field.len() {
        return Err(invalid("Worktree handoff value exceeds ustar limits"));
    }
    field.fill(b'0');
    let start = field.len() - digits.len() - 1;
    field[start..start + digits.len()].copy_from_slice(digits.as_bytes());
    field[field.len() - 1] = 0;
    Ok(())
}

fn pad_tar_entry(archive: &mut File, byte_count: u64) -> Result<(), EngineError> {
    let padding = (512 - byte_count % 512) % 512;
    if padding > 0 {
        archive.write_all(&[0_u8; 512][..padding as usize])?;
    }
    Ok(())
}

fn append_tar_end(archive: &mut File) -> Result<(), EngineError> {
    archive.write_all(&[0_u8; 1024])?;
    Ok(())
}

fn enforce_archive_limit(file: &File) -> Result<(), EngineError> {
    if file.metadata()?.len() > MAX_HANDOFF_ARCHIVE_BYTES {
        return Err(invalid("Worktree handoff archive exceeds its limit"));
    }
    Ok(())
}

fn run_git_bounded(
    cwd: &Path,
    args: &[&str],
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, EngineError> {
    check_cancelled(cancellation)?;
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| invalid(&format!("Could not start Git: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| invalid("Could not capture Git output"))?;
    enum ReadOutcome {
        Output(Vec<u8>),
        LimitExceeded,
        Failed(io::Error),
    }
    let (read_tx, read_rx) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut stdout = stdout;
        let mut output = Vec::with_capacity(limit.min(64 * 1024));
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    let _ = read_tx.send(ReadOutcome::Output(output));
                    return;
                }
                Ok(read) => {
                    let remaining = limit.saturating_add(1).saturating_sub(output.len());
                    output.extend_from_slice(&buffer[..read.min(remaining)]);
                    if output.len() > limit {
                        let _ = read_tx.send(ReadOutcome::LimitExceeded);
                        return;
                    }
                }
                Err(error) => {
                    let _ = read_tx.send(ReadOutcome::Failed(error));
                    return;
                }
            }
        }
    });
    let mut output = None;
    let mut status = None;
    loop {
        if cancellation.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(invalid("Worktree handoff capture was cancelled"));
        }
        match read_rx.try_recv() {
            Ok(ReadOutcome::Output(bytes)) => output = Some(bytes),
            Ok(ReadOutcome::LimitExceeded) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(invalid("Git path output exceeds the handoff limit"));
            }
            Ok(ReadOutcome::Failed(error)) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(error.into());
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) if output.is_none() => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(invalid("Could not capture Git output"));
            }
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
        if status.is_none() {
            status = child.try_wait()?;
        }
        if let Some(status) = status.as_ref()
            && let Some(output) = output.take()
        {
            let _ = reader.join();
            if !status.success() {
                return Err(invalid("Git could not capture the worktree handoff"));
            }
            return Ok(output);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), EngineError> {
    if cancellation.is_cancelled() {
        return Err(invalid("Worktree handoff capture was cancelled"));
    }
    Ok(())
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
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
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "symlink"));
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
        && before.mode() == after.mode()
        && before.nlink() == after.nlink()
}

#[cfg(not(unix))]
fn same_file_state(before: &Metadata, after: &Metadata) -> bool {
    before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
        && before.created().ok() == after.created().ok()
}

#[cfg(unix)]
fn is_executable(metadata: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &Metadata) -> bool {
    false
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn invalid(message: &str) -> EngineError {
    EngineError::Other(message.into())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(cwd: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap()
                .success()
        );
    }

    fn fixture() -> (tempfile::TempDir, String) {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "crew@example.com"]);
        git(temp.path(), &["config", "user.name", "Crew"]);
        std::fs::write(temp.path().join("kept.txt"), "base\n").unwrap();
        std::fs::write(temp.path().join("deleted.txt"), "delete\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "base"]);
        let sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(temp.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        (temp, sha)
    }

    #[test]
    fn captures_modified_untracked_and_deleted_paths_in_a_bounded_tar() {
        let (temp, base) = fixture();
        std::fs::write(temp.path().join("kept.txt"), "changed\n").unwrap();
        std::fs::write(temp.path().join("new.txt"), "new\n").unwrap();
        std::fs::remove_file(temp.path().join("deleted.txt")).unwrap();

        let snapshot = capture_worktree_handoff(temp.path(), &base).unwrap();
        assert_eq!(snapshot.base_sha, base);
        assert_eq!(snapshot.entry_count, 3);
        assert!(snapshot.byte_count <= MAX_HANDOFF_ARCHIVE_BYTES);
        assert_eq!(snapshot.manifest_sha256.len(), 64);

        let listing = Command::new("tar")
            .arg("-tf")
            .arg(snapshot.file.path())
            .output()
            .unwrap();
        assert!(listing.status.success());
        let listing = String::from_utf8(listing.stdout).unwrap();
        assert!(listing.contains("files/kept.txt"));
        assert!(listing.contains("files/new.txt"));
        assert!(listing.contains(MANIFEST_PATH));
        assert!(!listing.contains("files/deleted.txt"));
    }

    #[test]
    fn includes_ignored_omp_context_but_not_other_ignored_files() {
        let (temp, _) = fixture();
        std::fs::write(temp.path().join(".gitignore"), ".omx/\nignored.bin\n").unwrap();
        git(temp.path(), &["add", ".gitignore"]);
        git(temp.path(), &["commit", "-qm", "ignore context"]);
        let base = run_head(temp.path());
        std::fs::create_dir_all(temp.path().join(".omx/plans")).unwrap();
        std::fs::write(temp.path().join(".omx/plans/plan.md"), "plan\n").unwrap();
        std::fs::write(temp.path().join("ignored.bin"), "ignored\n").unwrap();

        let snapshot = capture_worktree_handoff(temp.path(), &base).unwrap();
        assert_eq!(snapshot.entry_count, 1);
        let listing = Command::new("tar")
            .arg("-tf")
            .arg(snapshot.file.path())
            .output()
            .unwrap();
        let listing = String::from_utf8(listing.stdout).unwrap();
        assert!(listing.contains("files/.omx/plans/plan.md"));
        assert!(!listing.contains("ignored.bin"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_hard_links_and_files_over_the_per_file_limit() {
        let (temp, base) = fixture();
        std::fs::write(temp.path().join("linked.txt"), "linked\n").unwrap();
        std::fs::hard_link(
            temp.path().join("linked.txt"),
            temp.path().join("linked-again.txt"),
        )
        .unwrap();
        assert!(capture_worktree_handoff(temp.path(), &base).is_err());
        std::fs::remove_file(temp.path().join("linked.txt")).unwrap();
        std::fs::remove_file(temp.path().join("linked-again.txt")).unwrap();

        let oversized = File::create(temp.path().join("oversized.bin")).unwrap();
        oversized.set_len(MAX_HANDOFF_FILE_BYTES + 1).unwrap();
        assert!(capture_worktree_handoff(temp.path(), &base).is_err());
    }
    #[test]
    fn rejects_path_bytes_before_retaining_the_full_git_output() {
        let mut output = Vec::new();
        for index in 0..MAX_HANDOFF_FILES {
            output.extend_from_slice(format!("{}/{index}", "a".repeat(4000)).as_bytes());
            output.push(0);
            if output.len() > MAX_TAR_LISTING_BYTES + 4096 {
                break;
            }
        }
        let mut paths = BTreeSet::new();
        let mut listing_bytes = 0;
        assert!(insert_git_paths(&mut paths, &mut listing_bytes, &output).is_err());
        assert!(listing_bytes <= MAX_TAR_LISTING_BYTES + 4096);
    }

    #[test]
    fn rejects_non_commit_bases_and_escaping_symlinks() {
        let (temp, _) = fixture();
        assert!(capture_worktree_handoff(temp.path(), "master").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("../../outside", temp.path().join("escape")).unwrap();
            assert!(capture_worktree_handoff(temp.path(), &run_head(temp.path())).is_err());
        }
    }

    #[test]
    fn rejects_a_cancelled_capture_before_allocating_archive_state() {
        let (temp, base) = fixture();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(capture_worktree_handoff_cancellable(temp.path(), &base, &cancellation).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_a_git_process_blocked_without_output() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;
        use std::time::Instant;

        let (temp, _) = fixture();
        let fifo = temp.path().join("blocked.fifo");
        let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        let cancellation = CancellationToken::new();
        let cancel_from_thread = cancellation.clone();
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancel_from_thread.cancel();
        });
        let started = Instant::now();
        assert!(
            run_git_bounded(
                temp.path(),
                &["hash-object", "blocked.fifo"],
                1024,
                &cancellation,
            )
            .is_err()
        );
        cancel.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn run_head(cwd: &Path) -> String {
        String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(cwd)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string()
    }
}
