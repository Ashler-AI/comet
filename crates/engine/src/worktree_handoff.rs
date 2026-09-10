//! Bounded, content-addressed capture of a source Git repository and its worktree.

use std::collections::BTreeSet;
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom, Write};
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
const REPOSITORY_PATH: &str = "source.bundle";

#[derive(Debug)]
pub(crate) struct WorktreeHandoffArchive {
    file: NamedTempFile,
    pub byte_count: u64,
    pub manifest_sha256: String,
    pub base_sha: String,
    pub cwd_relative_path: String,
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
    cwd_relative_path: String,
    repository: Repository,
    entries: Vec<ManifestEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    path: &'static str,
    sha256: String,
    byte_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    prerequisite_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shallow: Option<bool>,
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
pub(crate) fn capture_worktree_handoff(cwd: &Path) -> Result<WorktreeHandoffArchive, EngineError> {
    capture_worktree_handoff_cancellable(cwd, None, &CancellationToken::new())
}

pub(crate) fn capture_worktree_handoff_cancellable(
    cwd: &Path,
    remote_base: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<WorktreeHandoffArchive, EngineError> {
    check_cancelled(cancellation)?;
    if !cfg!(unix) {
        return Err(invalid(
            "Source repository handoff requires Unix process-group cancellation",
        ));
    }
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

    let cwd_relative_path = canonical_cwd
        .strip_prefix(&canonical_root)
        .map_err(|_| invalid("OMP session cwd is outside its Git worktree"))?
        .to_str()
        .ok_or_else(|| invalid("OMP session cwd is not UTF-8"))?
        .to_string();
    if !cwd_relative_path.is_empty() {
        validate_repo_path(&cwd_relative_path)?;
    }
    let resolved = run_git_bounded(
        &canonical_root,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        1024,
        cancellation,
    )?;
    let resolved = std::str::from_utf8(trim_ascii(&resolved))
        .map_err(|_| invalid("Git base commit is not UTF-8"))?;
    let base_sha = validate_base_sha(resolved)?.to_string();

    let mut paths = BTreeSet::new();
    let mut listing_bytes = 0_usize;
    let changed = run_git_bounded(
        &canonical_root,
        &["diff", "--name-only", "--no-renames", "-z", &base_sha, "--"],
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
    if archive.path().canonicalize()?.starts_with(&canonical_root) {
        return Err(invalid(
            "Worktree capture temporary files must be outside the source repository",
        ));
    }
    let repository = append_repository(
        archive.as_file_mut(),
        &canonical_root,
        &base_sha,
        remote_base,
        cancellation,
    )?;
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
        version: "crew.scaffold.worktree.v2",
        base_sha: base_sha.clone(),
        cwd_relative_path: cwd_relative_path.clone(),
        repository,
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
        base_sha,
        cwd_relative_path,
        entry_count,
    })
}

fn validate_base_sha(value: &str) -> Result<&str, EngineError> {
    let value = value.trim();
    if !(value.len() == 40 || value.len() == 64)
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid("Source HEAD did not resolve to an exact commit"));
    }
    Ok(value)
}

fn append_repository(
    archive: &mut File,
    root: &Path,
    base_sha: &str,
    remote_base: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<Repository, EngineError> {
    // Pin HEAD and traversal boundaries privately; never change source refs,
    // its index, or its shallow metadata (including for linked worktrees).
    let repository = tempfile::tempdir()?;
    if repository.path().canonicalize()?.starts_with(root) {
        return Err(invalid(
            "Worktree capture temporary files must be outside the source repository",
        ));
    }
    let objects = run_git_bounded(
        root,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ],
        16 * 1024,
        cancellation,
    )?;
    let objects = std::str::from_utf8(trim_ascii(&objects))
        .map_err(|_| invalid("Source Git object path is not UTF-8"))?;
    if objects.contains(['\n', '\r']) {
        return Err(invalid("Source Git object path contains a newline"));
    }
    run_git_bounded(
        repository.path(),
        &[
            "init",
            "--bare",
            "--quiet",
            "--template=",
            if base_sha.len() == 64 {
                "--object-format=sha256"
            } else {
                "--object-format=sha1"
            },
            ".",
        ],
        1024,
        cancellation,
    )?;
    std::fs::write(repository.path().join("HEAD"), format!("{base_sha}\n"))?;
    std::fs::write(
        repository.path().join("objects/info/alternates"),
        format!("{objects}\n"),
    )?;
    let shallow_path = run_git_bounded(
        root,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "shallow",
        ],
        16 * 1024,
        cancellation,
    )?;
    let shallow_path = std::str::from_utf8(trim_ascii(&shallow_path))
        .map_err(|_| invalid("Source Git shallow path is not UTF-8"))?;
    let mut source_boundaries = BTreeSet::new();
    match File::open(shallow_path) {
        Ok(file) => {
            let mut boundaries = Vec::new();
            file.take(MAX_GIT_PATH_OUTPUT_BYTES as u64 + 1)
                .read_to_end(&mut boundaries)?;
            if boundaries.len() > MAX_GIT_PATH_OUTPUT_BYTES {
                return Err(invalid("Source Git shallow metadata exceeds its limit"));
            }
            let text = std::str::from_utf8(&boundaries)
                .map_err(|_| invalid("Source Git shallow metadata is not UTF-8"))?;
            for boundary in text.lines() {
                if source_boundaries.len() >= MAX_HANDOFF_FILES {
                    return Err(invalid("Source Git has too many shallow boundaries"));
                }
                source_boundaries.insert(validate_base_sha(boundary)?.to_string());
            }
            std::fs::write(repository.path().join("shallow"), boundaries)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut prerequisite_sha = match remote_base {
        Some(remote_base) => {
            let remote_base = validate_base_sha(remote_base)?;
            // Missing/unrelated sandbox commits deliberately fall back to a
            // snapshot. Lazy fetching is disabled for this ancestry check.
            let known_ancestor = run_git_bounded(
                repository.path(),
                &["merge-base", "--is-ancestor", remote_base, base_sha],
                1024,
                cancellation,
            )
            .is_ok();
            check_cancelled(cancellation)?;
            known_ancestor.then(|| remote_base.to_string())
        }
        None => None,
    };
    if let Some(prerequisite) = prerequisite_sha.as_deref() {
        let exclusion = format!("^{prerequisite}");
        let commits = run_git_bounded(
            repository.path(),
            &["rev-list", "--boundary", "HEAD", &exclusion],
            MAX_GIT_PATH_OUTPUT_BYTES,
            cancellation,
        )?;
        // A merged branch can add an older exclusion boundary even in a full
        // clone. The receiver only guarantees the declared prerequisite, not
        // its ancestors. Source shallow boundaries also require a snapshot.
        if commits.split(|byte| *byte == b'\n').any(|commit| {
            if let Some(boundary) = commit.strip_prefix(b"-") {
                boundary != prerequisite.as_bytes()
            } else {
                std::str::from_utf8(commit).is_ok_and(|sha| source_boundaries.contains(sha))
            }
        }) {
            prerequisite_sha = None;
        }
    }
    if prerequisite_sha.as_deref() == Some(base_sha) {
        append_bytes(archive, REPOSITORY_PATH, &[], 0o600)?;
        return Ok(Repository {
            path: REPOSITORY_PATH,
            sha256: hex_sha256(&[]),
            byte_count: 0,
            prerequisite_sha,
            shallow: None,
        });
    }
    let shallow = prerequisite_sha.is_none().then_some(true);
    if shallow.is_some() {
        // A single exact commit and its tree, not the repository's history.
        std::fs::write(repository.path().join("shallow"), format!("{base_sha}\n"))?;
    }
    let exclusion = prerequisite_sha.as_ref().map(|sha| format!("^{sha}"));
    hydrate_missing_bundle_objects(root, repository.path(), exclusion.as_deref(), cancellation)?;

    let header_offset = archive.stream_position()?;
    append_tar_header(archive, REPOSITORY_PATH, 0, 0o600)?;
    let output = archive.try_clone()?;
    // Reserve padding/end markers; later entries and manifest have independent
    // pre-write capacity checks. No bundle-sized buffer or temporary pack on disk.
    let limit = MAX_HANDOFF_ARCHIVE_BYTES.saturating_sub(output.metadata()?.len() + 1535);
    let cancel_reader = cancellation.clone();
    let mut args = vec!["-c", "pack.threads=1", "bundle", "create", "-", "HEAD"];
    if let Some(exclusion) = exclusion.as_deref() {
        args.push(exclusion);
    }
    let (sha256, byte_count) =
        run_git_with_reader(repository.path(), &args, cancellation, move |input| {
            copy_bundle_bounded(input, output, limit, &cancel_reader)
        })?;
    let end = archive.stream_position()?;
    archive.seek(SeekFrom::Start(header_offset))?;
    append_tar_header(archive, REPOSITORY_PATH, byte_count, 0o600)?;
    archive.seek(SeekFrom::Start(end))?;
    pad_tar_entry(archive, byte_count)?;
    Ok(Repository {
        path: REPOSITORY_PATH,
        sha256,
        byte_count,
        prerequisite_sha,
        shallow,
    })
}

fn hydrate_missing_bundle_objects(
    source: &Path,
    repository: &Path,
    exclusion: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<(), EngineError> {
    let mut hydrated = BTreeSet::new();
    let objects = repository.join("objects");
    let limits = FetchLimits::default();
    loop {
        let mut args = vec![
            "rev-list",
            "--objects",
            "--no-object-names",
            "--missing=print",
            "HEAD",
        ];
        if let Some(exclusion) = exclusion {
            args.push(exclusion);
        }
        let listing = run_git_bounded(repository, &args, MAX_GIT_PATH_OUTPUT_BYTES, cancellation)?;
        let listing = std::str::from_utf8(&listing)
            .map_err(|_| invalid("Source Git object listing is not UTF-8"))?;
        let Some(object) = listing.lines().find_map(|line| line.strip_prefix('?')) else {
            return Ok(());
        };
        let object = validate_base_sha(object)?;
        if hydrated.len() >= limits.objects || !hydrated.insert(object.to_string()) {
            return Err(invalid(
                "Source Git objects could not be hydrated within the handoff limit",
            ));
        }
        // Preserve the source's remote/auth configuration, but redirect every
        // child fetch into private storage. Its alternates provide source reads.
        run_git_with_reader_fetching(
            source,
            &["cat-file", "-s", object],
            cancellation,
            Some((&objects, limits, true)),
            |mut input| {
                let mut size = String::new();
                input.by_ref().take(64).read_to_string(&mut size)?;
                let size = size
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| invalid("Invalid hydrated Git object size"))?;
                if size > MAX_HANDOFF_ARCHIVE_BYTES {
                    return Err(invalid("Source Git object exceeds the handoff limit"));
                }
                Ok(())
            },
        )?;
        // A server may include more than the requested object. Account every
        // local pack before another fetch or the final bundle, not just stdout.
        verify_fetched_objects(repository, &objects, limits, cancellation)?;
    }
}

#[derive(Clone, Copy)]
struct FetchLimits {
    file_bytes: u64,
    storage_bytes: u64,
    memory_bytes: u64,
    objects: usize,
    expanded_bytes: u64,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            file_bytes: MAX_HANDOFF_ARCHIVE_BYTES,
            storage_bytes: 512 * 1024 * 1024,
            memory_bytes: 2 * 1024 * 1024 * 1024,
            objects: 250_000,
            expanded_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

// Inspect only the private directory, never follow its source alternates. This
// runs during fetch (10ms polling) and again after exit: aggregate limits are
// observed/cancelled, while RLIMIT_FSIZE is the hard per-file write boundary.
fn inspect_fetch_storage(
    objects: &Path,
    limits: FetchLimits,
    cancellation: &CancellationToken,
) -> Result<Vec<PathBuf>, EngineError> {
    let mut files = Vec::new();
    let pack_directory = objects.join("pack");
    let mut bytes = 0_u64;
    let mut entries = 0_usize;
    let mut pack_objects = 0_u64;
    let mut directories = vec![(objects.to_path_buf(), 0)];
    while let Some((directory, depth)) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            check_cancelled(cancellation)?;
            let entry = entry?;
            entries += 1;
            if entries > limits.objects.saturating_add(260) {
                return Err(invalid("Fetched Git storage has too many entries"));
            }
            let path = entry.path();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if metadata.is_dir() && depth == 0 {
                directories.push((path, depth + 1));
                continue;
            }
            if !metadata.is_file() {
                return Err(invalid("Unexpected fetched Git storage entry"));
            }
            bytes = bytes.saturating_add(metadata.len());
            if metadata.len() > limits.file_bytes || bytes > limits.storage_bytes {
                return Err(invalid("Fetched Git storage exceeds the handoff limit"));
            }
            if path.parent() == Some(pack_directory.as_path()) {
                let mut header = [0_u8; 12];
                match open_regular_nofollow(&path) {
                    Ok(mut file) => {
                        if file.read_exact(&mut header).is_ok() && &header[..4] == b"PACK" {
                            pack_objects +=
                                u32::from_be_bytes(header[8..12].try_into().unwrap()) as u64;
                            if pack_objects > limits.objects as u64 {
                                return Err(invalid("Fetched Git packs contain too many objects"));
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            files.push(path);
        }
    }
    Ok(files)
}

fn verify_fetched_objects(
    repository: &Path,
    objects: &Path,
    limits: FetchLimits,
    cancellation: &CancellationToken,
) -> Result<(), EngineError> {
    let files: BTreeSet<_> = inspect_fetch_storage(objects, limits, cancellation)?
        .into_iter()
        .collect();
    let alternates = objects.join("info/alternates");
    let pack_directory = objects.join("pack");
    let mut count = 0_usize;
    let mut expanded = 0_u64;
    for path in &files {
        if path == &alternates {
            continue;
        }
        if path.parent() != Some(pack_directory.as_path()) {
            return Err(invalid("Unexpected loose object in isolated Git fetch"));
        }
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("pack" | "promisor" | "rev")
                if files.contains(&path.with_extension("idx"))
                    && files.contains(&path.with_extension("pack")) =>
            {
                continue;
            }
            Some("idx") if files.contains(&path.with_extension("pack")) => {}
            _ => return Err(invalid("Incomplete isolated Git fetch")),
        }
        let path = path
            .to_str()
            .ok_or_else(|| invalid("Git pack path is not UTF-8"))?;
        let (pack_count, pack_bytes) = run_git_with_reader_fetching(
            repository,
            &["verify-pack", "-v", path],
            cancellation,
            Some((objects, limits, false)),
            move |input| {
                use std::io::BufRead as _;
                let mut input = io::BufReader::new(input);
                let mut line = String::new();
                let mut count = 0_usize;
                let mut expanded = 0_u64;
                loop {
                    line.clear();
                    if std::io::Read::by_ref(&mut input)
                        .take(1025)
                        .read_line(&mut line)?
                        == 0
                    {
                        return Ok((count, expanded));
                    }
                    if line.len() > 1024 {
                        return Err(invalid("Git object accounting output exceeds its limit"));
                    }
                    let mut fields = line.split_whitespace();
                    let Some(sha) = fields.next() else { continue };
                    if validate_base_sha(sha).is_err() {
                        continue;
                    }
                    let _kind = fields.next();
                    let size = fields
                        .next()
                        .and_then(|size| size.parse::<u64>().ok())
                        .ok_or_else(|| invalid("Invalid fetched Git object size"))?;
                    count += 1;
                    expanded = expanded.saturating_add(size);
                    if size > MAX_HANDOFF_ARCHIVE_BYTES
                        || count > limits.objects
                        || expanded > limits.expanded_bytes
                    {
                        return Err(invalid(
                            "Fetched Git objects exceed the expanded handoff limit",
                        ));
                    }
                }
            },
        )?;
        count = count.saturating_add(pack_count);
        expanded = expanded.saturating_add(pack_bytes);
        if count > limits.objects || expanded > limits.expanded_bytes {
            return Err(invalid(
                "Fetched Git objects exceed the expanded handoff limit",
            ));
        }
    }
    Ok(())
}

fn copy_bundle_bounded(
    mut input: impl Read,
    mut output: impl Write,
    limit: u64,
    cancellation: &CancellationToken,
) -> Result<(String, u64), EngineError> {
    let mut digest = Sha256::new();
    let mut byte_count = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_cancelled(cancellation)?;
        let read = input.read(&mut buffer)?;
        if read == 0 {
            return Ok((format!("{:x}", digest.finalize()), byte_count));
        }
        if read as u64 > limit.saturating_sub(byte_count) {
            return Err(invalid(
                "Source Git bundle exceeds the handoff archive limit",
            ));
        }
        output.write_all(&buffer[..read])?;
        digest.update(&buffer[..read]);
        byte_count += read as u64;
    }
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
    ensure_archive_capacity(archive, 512 + before.len().div_ceil(512) * 512)?;
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
    ensure_archive_capacity(archive, (bytes.len() as u64).div_ceil(512) * 512)?;
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
    ensure_archive_capacity(archive, 512)?;
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
    ensure_archive_capacity(archive, padding)?;
    if padding > 0 {
        archive.write_all(&[0_u8; 512][..padding as usize])?;
    }
    Ok(())
}

fn append_tar_end(archive: &mut File) -> Result<(), EngineError> {
    ensure_archive_capacity(archive, 1024)?;
    archive.write_all(&[0_u8; 1024])?;
    Ok(())
}

fn enforce_archive_limit(file: &File) -> Result<(), EngineError> {
    if file.metadata()?.len() > MAX_HANDOFF_ARCHIVE_BYTES {
        return Err(invalid("Worktree handoff archive exceeds its limit"));
    }
    Ok(())
}

fn ensure_archive_capacity(file: &File, additional: u64) -> Result<(), EngineError> {
    if additional > MAX_HANDOFF_ARCHIVE_BYTES.saturating_sub(file.metadata()?.len()) {
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
    run_git_with_reader(cwd, args, cancellation, move |mut stdout| {
        let mut output = Vec::with_capacity(limit.min(64 * 1024));
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = stdout.read(&mut buffer)?;
            if read == 0 {
                return Ok(output);
            }
            if read > limit.saturating_sub(output.len()) {
                return Err(invalid("Git path output exceeds the handoff limit"));
            }
            output.extend_from_slice(&buffer[..read]);
        }
    })
}

fn run_git_with_reader<T: Send + 'static>(
    cwd: &Path,
    args: &[&str],
    cancellation: &CancellationToken,
    consume: impl FnOnce(std::process::ChildStdout) -> Result<T, EngineError> + Send + 'static,
) -> Result<T, EngineError> {
    run_git_with_reader_fetching(cwd, args, cancellation, None, consume)
}

#[cfg(target_os = "macos")]
fn inspect_fetch_memory(group: u32, limit: u64) -> Result<(), EngineError> {
    // Darwin's RLIMIT_AS/RSS are not hard memory limits. Enforce observed total
    // group RSS instead; between-sample overshoot remains possible. Fixed-size
    // buffers bound inspection and reject unexpectedly large process groups.
    #[repr(C)]
    #[derive(Default)]
    struct TaskInfo {
        sizes_and_times: [u64; 6],
        counters: [i32; 12],
    }
    unsafe extern "C" {
        fn proc_listpgrppids(
            group: libc::pid_t,
            buffer: *mut libc::c_void,
            size: libc::c_int,
        ) -> libc::c_int;
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            size: libc::c_int,
        ) -> libc::c_int;
    }
    let mut pids = [0 as libc::pid_t; 65];
    let count = unsafe {
        proc_listpgrppids(
            group as libc::pid_t,
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as libc::c_int,
        )
    };
    if count < 0 || count as usize >= pids.len() {
        return Err(invalid("Could not bound Git fetch process group"));
    }
    let mut resident = 0_u64;
    for pid in &pids[..count as usize] {
        if *pid <= 0 {
            return Err(invalid("Invalid Git fetch process group member"));
        }
        let mut info = TaskInfo::default();
        let size = std::mem::size_of::<TaskInfo>() as libc::c_int;
        let read = unsafe { proc_pidinfo(*pid, 4, 0, (&mut info as *mut TaskInfo).cast(), size) };
        if read != size {
            // Exited members (including zombies) have no task memory to inspect.
            if io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(invalid("Could not inspect Git fetch process memory"));
        }
        resident = resident.saturating_add(info.sizes_and_times[1]);
        if resident > limit {
            return Err(invalid(
                "Git fetch process memory exceeds the handoff limit",
            ));
        }
    }
    Ok(())
}

fn run_git_with_reader_fetching<T: Send + 'static>(
    cwd: &Path,
    args: &[&str],
    cancellation: &CancellationToken,
    fetch: Option<(&Path, FetchLimits, bool)>,
    consume: impl FnOnce(std::process::ChildStdout) -> Result<T, EngineError> + Send + 'static,
) -> Result<T, EngineError> {
    check_cancelled(cancellation)?;
    if fetch.is_some() && !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Err(invalid(
            "Isolated Git hydration requires supported subprocess memory enforcement",
        ));
    }
    let mut command = Command::new("git");
    if fetch.is_some() {
        command.args([
            "-c",
            "pack.threads=1",
            "-c",
            "index.threads=1",
            "-c",
            "fetch.unpackLimit=0",
            "-c",
            "transfer.unpackLimit=0",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ]);
    }
    command
        .args(args)
        .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_SHALLOW_FILE")
        .env(
            "GIT_NO_LAZY_FETCH",
            if fetch.is_some_and(|(_, _, lazy)| lazy) {
                "0"
            } else {
                "1"
            },
        )
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some((objects, _, _)) = fetch {
        // Apply only this explicitly controlled override AFTER clearing caller
        // environment; Git's promisor fetch/index-pack children inherit it.
        command.env("GIT_OBJECT_DIRECTORY", objects);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
        if let Some((_, limits, _)) = fetch {
            // pre_exec is limited to async-signal-safe syscalls and stack data.
            unsafe {
                command.pre_exec(move || {
                    for (resource, value) in [
                        (libc::RLIMIT_FSIZE, limits.file_bytes),
                        #[cfg(target_os = "linux")]
                        (libc::RLIMIT_AS, limits.memory_bytes),
                        (libc::RLIMIT_CORE, 0),
                    ] {
                        let limit = libc::rlimit {
                            rlim_cur: value as libc::rlim_t,
                            rlim_max: value as libc::rlim_t,
                        };
                        if libc::setrlimit(resource, &limit) != 0 {
                            return Err(io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| invalid(&format!("Could not start Git: {error}")))?;
    let stdout = child.stdout.take().expect("Git stdout is piped");
    let (read_tx, read_rx) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let _ = read_tx.send(consume(stdout));
    });
    let mut reaped = false;
    let result = (|| {
        let mut output = None;
        loop {
            check_cancelled(cancellation)?;
            if let Some((objects, limits, _)) = fetch {
                #[cfg(target_os = "macos")]
                inspect_fetch_memory(child.id(), limits.memory_bytes)?;
                inspect_fetch_storage(objects, limits, cancellation)?;
            }
            match read_rx.try_recv() {
                Ok(result) => output = Some(result?),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) if output.is_none() => {
                    return Err(invalid("Could not capture Git output"));
                }
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
            // Keep the leader waitable while output/descendants are pending, so
            // cancellation cannot address a PGID reused after an early reap.
            if output.is_some() {
                if let Some((objects, limits, _)) = fetch {
                    inspect_fetch_storage(objects, limits, cancellation)?;
                }
                if let Some(status) = child.try_wait()? {
                    reaped = true;
                    if !status.success() {
                        return Err(invalid("Git could not capture the worktree handoff"));
                    }
                    return Ok(output.take().expect("Git output is complete"));
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() && !reaped {
        // Git bundle spawns pack-objects; killing only Git leaves the packer
        // alive with our stdout pipe open and can hang the reader join.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = reader.join();
    result
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
                .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false")
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
                .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false")
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

        let snapshot = capture_worktree_handoff(temp.path()).unwrap();
        assert_eq!(snapshot.base_sha, base);
        assert_eq!(snapshot.entry_count, 3);
        assert!(snapshot.byte_count <= MAX_HANDOFF_ARCHIVE_BYTES);

        let listing = Command::new("tar")
            .arg("-tf")
            .arg(snapshot.file.path())
            .output()
            .unwrap();
        assert!(listing.status.success());
        let listing = String::from_utf8(listing.stdout).unwrap();
        assert!(listing.contains("files/kept.txt"));
        assert!(listing.contains(REPOSITORY_PATH));
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
        std::fs::create_dir_all(temp.path().join(".omx/plans")).unwrap();
        std::fs::write(temp.path().join(".omx/plans/plan.md"), "plan\n").unwrap();
        std::fs::write(temp.path().join("ignored.bin"), "ignored\n").unwrap();

        let snapshot = capture_worktree_handoff(temp.path()).unwrap();
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
        let (temp, _) = fixture();
        std::fs::write(temp.path().join("linked.txt"), "linked\n").unwrap();
        std::fs::hard_link(
            temp.path().join("linked.txt"),
            temp.path().join("linked-again.txt"),
        )
        .unwrap();
        assert!(capture_worktree_handoff(temp.path()).is_err());
        std::fs::remove_file(temp.path().join("linked.txt")).unwrap();
        std::fs::remove_file(temp.path().join("linked-again.txt")).unwrap();

        let oversized = File::create(temp.path().join("oversized.bin")).unwrap();
        oversized.set_len(MAX_HANDOFF_FILE_BYTES + 1).unwrap();
        assert!(capture_worktree_handoff(temp.path()).is_err());
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
    fn rejects_unborn_head() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        assert!(capture_worktree_handoff(temp.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_escaping_symlinks() {
        let (temp, _) = fixture();
        std::os::unix::fs::symlink("../../outside", temp.path().join("escape")).unwrap();
        assert!(capture_worktree_handoff(temp.path()).is_err());
    }

    #[test]
    fn rejects_a_cancelled_capture_before_allocating_archive_state() {
        let (temp, _) = fixture();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(capture_worktree_handoff_cancellable(temp.path(), None, &cancellation).is_err());
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

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_git_descendants_holding_stdout_open() {
        let (temp, _) = fixture();
        let cancellation = CancellationToken::new();
        let cancel_from_thread = cancellation.clone();
        let cancel = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            cancel_from_thread.cancel();
        });
        let started = std::time::Instant::now();
        assert!(
            run_git_bounded(
                temp.path(),
                &["-c", "alias.handoff-block=!sleep 30", "handoff-block"],
                1024,
                &cancellation,
            )
            .is_err()
        );
        cancel.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn captures_linked_worktree_head_instead_of_main_checkout_head() {
        let (source, main_head) = fixture();
        let directory = tempfile::tempdir().unwrap();
        let worktree = directory.path().join("linked");
        git(
            source.path(),
            &[
                "worktree",
                "add",
                "--detach",
                "-q",
                worktree.to_str().unwrap(),
            ],
        );
        std::fs::write(worktree.join("only-linked.txt"), "linked\n").unwrap();
        git(&worktree, &["add", "."]);
        git(&worktree, &["commit", "-qm", "linked only"]);
        let linked_head = run_head(&worktree);
        let snapshot = capture_worktree_handoff(&worktree).unwrap();
        assert_eq!(snapshot.base_sha, linked_head);
        assert_ne!(snapshot.base_sha, main_head);
        assert!(snapshot.cwd_relative_path.is_empty());
        assert_eq!(snapshot.entry_count, 0);
        assert_eq!(run_head(source.path()), main_head);
        let unpacked = tempfile::tempdir().unwrap();
        assert!(
            Command::new("tar")
                .arg("-xf")
                .arg(snapshot.file.path())
                .arg("-C")
                .arg(unpacked.path())
                .status()
                .unwrap()
                .success()
        );
        let restored = restore_snapshot(unpacked.path(), &linked_head);
        assert_eq!(run_head(&restored), linked_head);
    }

    #[test]
    fn transfers_source_snapshot_and_nested_dirty_worktree_without_destination_base() {
        let (source, _) = fixture();
        let (platform, _) = fixture();
        std::fs::write(platform.path().join("platform.txt"), "unrelated platform\n").unwrap();
        git(platform.path(), &["add", "."]);
        git(platform.path(), &["commit", "-qm", "platform only"]);
        let platform_head = run_head(platform.path());
        let nested = source.path().join("packages/app");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("tracked.txt"), "source commit\n").unwrap();
        git(source.path(), &["add", "."]);
        git(source.path(), &["commit", "-qm", "source only"]);
        let source_head = run_head(source.path());
        git(source.path(), &["checkout", "--detach", "-q"]);
        std::fs::write(nested.join("tracked.txt"), "staged change\n").unwrap();
        git(source.path(), &["add", "."]);
        std::fs::write(nested.join("tracked.txt"), "dirty source\n").unwrap();
        std::fs::write(nested.join("new.txt"), "untracked source\n").unwrap();
        std::fs::remove_file(source.path().join("deleted.txt")).unwrap();
        let refs_before = git_output(source.path(), &["show-ref"]);
        let index_before = std::fs::read(source.path().join(".git/index")).unwrap();
        let snapshot = capture_worktree_handoff(&nested).unwrap();
        assert_eq!(snapshot.base_sha, source_head);
        assert_ne!(snapshot.base_sha, platform_head);
        assert_eq!(snapshot.cwd_relative_path, "packages/app");
        assert_eq!(snapshot.entry_count, 3);
        assert_eq!(git_output(source.path(), &["show-ref"]), refs_before);
        assert_eq!(
            std::fs::read(source.path().join(".git/index")).unwrap(),
            index_before
        );
        assert_eq!(run_head(source.path()), source_head);

        let unpacked = tempfile::tempdir().unwrap();
        assert!(
            Command::new("tar")
                .arg("-xf")
                .arg(snapshot.file.path())
                .arg("-C")
                .arg(unpacked.path())
                .status()
                .unwrap()
                .success()
        );
        let manifest_bytes = std::fs::read(unpacked.path().join(MANIFEST_PATH)).unwrap();
        assert_eq!(hex_sha256(&manifest_bytes), snapshot.manifest_sha256);
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
        assert_eq!(manifest["version"], "crew.scaffold.worktree.v2");
        assert_eq!(manifest["baseSha"], source_head);
        assert_eq!(manifest["cwdRelativePath"], "packages/app");
        let bundle = std::fs::read(unpacked.path().join(REPOSITORY_PATH)).unwrap();
        assert_eq!(manifest["repository"]["sha256"], hex_sha256(&bundle));
        assert_eq!(manifest["repository"]["byteCount"], bundle.len() as u64);
        // Delete the source to prove the exact HEAD tree is self-contained.
        source.close().unwrap();
        let restored = restore_snapshot(unpacked.path(), &source_head);
        assert_eq!(run_head(&restored), source_head);
        assert_eq!(
            git_output(&restored, &["rev-list", "--count", "HEAD"]),
            b"1\n"
        );
        assert_eq!(
            std::fs::read_to_string(restored.join("packages/app/tracked.txt")).unwrap(),
            "source commit\n"
        );
        assert_eq!(
            std::fs::read_to_string(unpacked.path().join("files/packages/app/tracked.txt"))
                .unwrap(),
            "dirty source\n"
        );
        assert_eq!(
            std::fs::read_to_string(unpacked.path().join("files/packages/app/new.txt")).unwrap(),
            "untracked source\n"
        );
        assert!(
            manifest["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry["kind"] == "delete" && entry["path"] == "deleted.txt" })
        );
        assert_eq!(run_head(platform.path()), platform_head);
    }

    #[test]
    fn rejects_bundle_overflow_before_writing_beyond_the_budget() {
        let cancellation = CancellationToken::new();
        let input = vec![0_u8; 64 * 1024 + 1];
        let mut output = Vec::new();
        assert!(copy_bundle_bounded(&input[..], &mut output, 64 * 1024, &cancellation).is_err());
        assert_eq!(output.len(), 64 * 1024);
        let mut exact = Vec::new();
        let (digest, count) =
            copy_bundle_bounded(&input[..], &mut exact, input.len() as u64, &cancellation).unwrap();
        assert_eq!(exact, input);
        assert_eq!(count, input.len() as u64);
        assert_eq!(digest, hex_sha256(&input));
        cancellation.cancel();
        let mut cancelled_output = Vec::new();
        assert!(
            copy_bundle_bounded(&input[..], &mut cancelled_output, u64::MAX, &cancellation)
                .is_err()
        );
        assert!(cancelled_output.is_empty());
    }

    #[test]
    fn archive_limit_includes_tar_headers_and_padding_before_writes() {
        let mut file = tempfile::tempfile().unwrap();
        file.set_len(MAX_HANDOFF_ARCHIVE_BYTES - 512).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        assert!(append_bytes(&mut file, "files/overflow", b"x", 0o600).is_err());
        assert!(file.metadata().unwrap().len() <= MAX_HANDOFF_ARCHIVE_BYTES);
    }

    fn unpack(snapshot: &WorktreeHandoffArchive) -> (tempfile::TempDir, serde_json::Value) {
        let unpacked = tempfile::tempdir().unwrap();
        assert!(
            Command::new("tar")
                .arg("-xf")
                .arg(snapshot.file.path())
                .arg("-C")
                .arg(unpacked.path())
                .status()
                .unwrap()
                .success()
        );
        let manifest =
            serde_json::from_slice(&std::fs::read(unpacked.path().join(MANIFEST_PATH)).unwrap())
                .unwrap();
        (unpacked, manifest)
    }

    fn restore_snapshot(unpacked: &Path, head: &str) -> PathBuf {
        let restored = unpacked.join("restored");
        std::fs::create_dir(&restored).unwrap();
        git(&restored, &["init", "-q"]);
        std::fs::write(restored.join(".git/shallow"), format!("{head}\n")).unwrap();
        git(&restored, &["bundle", "unbundle", "../source.bundle"]);
        git(&restored, &["checkout", "--detach", "-q", head]);
        git(&restored, &["fsck", "--strict"]);
        restored
    }

    #[test]
    fn snapshot_omits_large_deleted_history_and_unknown_prerequisite() {
        let (source, _) = fixture();
        let mut large = File::create(source.path().join("historical.bin")).unwrap();
        for index in 0..65536_u64 {
            large
                .write_all(&Sha256::digest(index.to_le_bytes()))
                .unwrap();
        }
        drop(large);
        git(source.path(), &["add", "."]);
        git(source.path(), &["commit", "-qm", "large historical object"]);
        git(source.path(), &["rm", "-q", "historical.bin"]);
        git(
            source.path(),
            &["commit", "-qm", "remove historical object"],
        );
        let head = run_head(source.path());
        let refs = git_output(source.path(), &["show-ref"]);
        let snapshot = capture_worktree_handoff_cancellable(
            source.path(),
            Some("1111111111111111111111111111111111111111"),
            &CancellationToken::new(),
        )
        .unwrap();
        let (unpacked, manifest) = unpack(&snapshot);
        assert_eq!(manifest["repository"]["shallow"], true);
        assert!(manifest["repository"].get("prerequisiteSha").is_none());
        assert!(manifest["repository"]["byteCount"].as_u64().unwrap() < 16 * 1024);
        assert_eq!(git_output(source.path(), &["show-ref"]), refs);
        assert!(!source.path().join(".git/shallow").exists());
        source.close().unwrap();
        let restored = restore_snapshot(unpacked.path(), &head);
        assert_eq!(run_head(&restored), head);
        assert_eq!(
            git_output(&restored, &["rev-list", "--count", "HEAD"]),
            b"1\n"
        );
        assert_eq!(std::fs::read(restored.join("kept.txt")).unwrap(), b"base\n");
    }

    #[test]
    fn delta_requires_and_restores_against_exact_ancestor() {
        let (source, prerequisite) = fixture();
        let platform = tempfile::tempdir().unwrap();
        git(
            platform.path(),
            &["clone", "-q", source.path().to_str().unwrap(), "repo"],
        );
        std::fs::write(source.path().join("kept.txt"), "delta\n").unwrap();
        git(source.path(), &["add", "."]);
        git(source.path(), &["commit", "-qm", "delta"]);
        let head = run_head(source.path());
        let snapshot = capture_worktree_handoff_cancellable(
            source.path(),
            Some(&prerequisite),
            &CancellationToken::new(),
        )
        .unwrap();
        let (unpacked, manifest) = unpack(&snapshot);
        assert_eq!(manifest["repository"]["prerequisiteSha"], prerequisite);
        assert!(manifest["repository"].get("shallow").is_none());
        let restored = platform.path().join("repo");
        let bundle = unpacked.path().join(REPOSITORY_PATH);
        source.close().unwrap();
        git(&restored, &["bundle", "verify", bundle.to_str().unwrap()]);
        git(&restored, &["bundle", "unbundle", bundle.to_str().unwrap()]);
        git(&restored, &["checkout", "--detach", "-q", &head]);
        git(&restored, &["fsck", "--strict"]);
        assert_eq!(run_head(&restored), head);
        assert_eq!(
            std::fs::read(restored.join("kept.txt")).unwrap(),
            b"delta\n"
        );
        assert_eq!(
            git_output(&restored, &["rev-parse", "HEAD^"]),
            format!("{prerequisite}\n").as_bytes()
        );
    }

    #[test]
    fn equal_head_sends_empty_repository_with_dirty_overlay() {
        let (source, head) = fixture();
        std::fs::write(source.path().join("kept.txt"), "dirty\n").unwrap();
        let snapshot = capture_worktree_handoff_cancellable(
            source.path(),
            Some(&head),
            &CancellationToken::new(),
        )
        .unwrap();
        let (unpacked, manifest) = unpack(&snapshot);
        assert_eq!(manifest["repository"]["prerequisiteSha"], head);
        assert_eq!(manifest["repository"]["byteCount"], 0);
        assert_eq!(manifest["repository"]["sha256"], hex_sha256(&[]));
        assert_eq!(
            std::fs::metadata(unpacked.path().join(REPOSITORY_PATH))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            std::fs::read(unpacked.path().join("files/kept.txt")).unwrap(),
            b"dirty\n"
        );
    }

    fn object_store_fingerprint(objects: &Path) -> BTreeSet<(PathBuf, String)> {
        let mut pending = vec![objects.to_path_buf()];
        let mut result = BTreeSet::new();
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    pending.push(path);
                } else {
                    result.insert((
                        path.strip_prefix(objects).unwrap().to_path_buf(),
                        hex_sha256(&std::fs::read(path).unwrap()),
                    ));
                }
            }
        }
        result
    }

    #[test]
    fn rejects_oversized_isolated_fetch_without_mutating_source_objects() {
        let (origin, _) = fixture();
        let mut large = File::create(origin.path().join("large.bin")).unwrap();
        for index in 0..32768_u64 {
            large
                .write_all(&Sha256::digest(index.to_le_bytes()))
                .unwrap();
        }
        drop(large);
        git(origin.path(), &["add", "."]);
        git(origin.path(), &["commit", "-qm", "large blob"]);
        git(origin.path(), &["config", "uploadpack.allowFilter", "true"]);
        git(
            origin.path(),
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        let directory = tempfile::tempdir().unwrap();
        git(
            directory.path(),
            &[
                "clone",
                "-q",
                "--depth=1",
                "--filter=blob:none",
                "--no-checkout",
                &format!("file://{}", origin.path().display()),
                "partial",
            ],
        );
        let source = directory.path().join("partial");
        let source_objects = source.join(".git/objects");
        let before = object_store_fingerprint(&source_objects);
        let object =
            String::from_utf8(git_output(origin.path(), &["rev-parse", "HEAD:large.bin"])).unwrap();
        let object = object.trim();
        let cancellation = CancellationToken::new();
        // Separate real fetches exercise hard per-file writes, observed aggregate
        // storage, and subprocess memory, not just a cat-file output counter.
        for limits in [
            FetchLimits {
                file_bytes: 64 * 1024,
                ..FetchLimits::default()
            },
            FetchLimits {
                storage_bytes: 64 * 1024,
                ..FetchLimits::default()
            },
            FetchLimits {
                memory_bytes: 1,
                ..FetchLimits::default()
            },
        ] {
            let repository = tempfile::tempdir().unwrap();
            git(repository.path(), &["init", "--bare", "-q"]);
            let objects = repository.path().join("objects");
            std::fs::write(
                objects.join("info/alternates"),
                format!("{}\n", source_objects.display()),
            )
            .unwrap();
            let result = run_git_with_reader_fetching(
                &source,
                &["cat-file", "-s", object],
                &cancellation,
                Some((&objects, limits, true)),
                |mut output| {
                    io::copy(&mut output, &mut io::sink())?;
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert_eq!(object_store_fingerprint(&source_objects), before);
            // RLIMIT_FSIZE is a hard bound even when aggregate polling has not
            // sampled the incoming file yet.
            for entry in std::fs::read_dir(objects.join("pack")).unwrap() {
                assert!(entry.unwrap().metadata().unwrap().len() <= limits.file_bytes);
            }
            let path = repository.path().to_path_buf();
            repository.close().unwrap();
            assert!(!path.exists());
        }
        // A valid fetch can still exceed the receiver's expanded-object budget;
        // verify the stored pack rather than trusting the requested object's size.
        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "--bare", "-q"]);
        let objects = repository.path().join("objects");
        std::fs::write(
            objects.join("info/alternates"),
            format!("{}\n", source_objects.display()),
        )
        .unwrap();
        run_git_with_reader_fetching(
            &source,
            &["cat-file", "-s", object],
            &cancellation,
            Some((&objects, FetchLimits::default(), true)),
            |mut output| {
                io::copy(&mut output, &mut io::sink())?;
                Ok(())
            },
        )
        .unwrap();
        let error = verify_fetched_objects(
            repository.path(),
            &objects,
            FetchLimits {
                expanded_bytes: 64 * 1024,
                ..FetchLimits::default()
            },
            &cancellation,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expanded handoff limit"));
        assert_eq!(object_store_fingerprint(&source_objects), before);
    }

    #[test]
    fn captures_shallow_partial_clone_without_exporting_promisor_config() {
        let (origin, _) = fixture();
        std::fs::write(origin.path().join("kept.txt"), "current\n").unwrap();
        git(origin.path(), &["add", "."]);
        git(origin.path(), &["commit", "-qm", "current head"]);
        git(origin.path(), &["config", "uploadpack.allowFilter", "true"]);
        git(
            origin.path(),
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        let directory = tempfile::tempdir().unwrap();
        let url = format!("file://{}", origin.path().display());
        git(
            directory.path(),
            &[
                "clone",
                "-q",
                "--depth=1",
                "--filter=blob:none",
                "--no-checkout",
                &url,
                "partial",
            ],
        );
        let source = directory.path().join("partial");
        git(
            &source,
            &["sparse-checkout", "set", "--no-cone", "/kept.txt"],
        );
        git(&source, &["checkout", "-q"]);
        let head = run_head(&source);
        let missing = git_output(
            &source,
            &["rev-list", "--objects", "--missing=print", "HEAD"],
        );
        assert!(
            missing
                .split(|byte| *byte == b'\n')
                .any(|line| line.starts_with(b"?"))
        );
        let shallow_before = std::fs::read(source.join(".git/shallow")).unwrap();
        let config_before = std::fs::read(source.join(".git/config")).unwrap();
        let refs_before = git_output(&source, &["show-ref"]);
        let index_before = std::fs::read(source.join(".git/index")).unwrap();
        let objects_before = object_store_fingerprint(&source.join(".git/objects"));
        let snapshot = capture_worktree_handoff(&source).unwrap();
        assert_eq!(
            std::fs::read(source.join(".git/shallow")).unwrap(),
            shallow_before
        );
        assert_eq!(
            std::fs::read(source.join(".git/config")).unwrap(),
            config_before
        );
        assert_eq!(git_output(&source, &["show-ref"]), refs_before);
        assert_eq!(
            std::fs::read(source.join(".git/index")).unwrap(),
            index_before
        );
        assert_eq!(
            object_store_fingerprint(&source.join(".git/objects")),
            objects_before
        );
        let (unpacked, manifest) = unpack(&snapshot);
        assert_eq!(manifest["repository"]["shallow"], true);
        origin.close().unwrap();
        directory.close().unwrap();
        let restored = restore_snapshot(unpacked.path(), &head);
        assert_eq!(
            std::fs::read(restored.join("kept.txt")).unwrap(),
            b"current\n"
        );
        assert_eq!(
            std::fs::read(restored.join("deleted.txt")).unwrap(),
            b"delete\n"
        );
        assert_eq!(
            git_output(&restored, &["rev-list", "--count", "HEAD"]),
            b"1\n"
        );
    }

    #[test]
    fn rejects_missing_non_promisor_blob_instead_of_emitting_incomplete_snapshot() {
        let (source, _) = fixture();
        let blob = git_output(source.path(), &["rev-parse", "HEAD:deleted.txt"]);
        let blob = std::str::from_utf8(&blob).unwrap().trim();
        std::fs::remove_file(
            source
                .path()
                .join(".git/objects")
                .join(&blob[..2])
                .join(&blob[2..]),
        )
        .unwrap();
        assert!(capture_worktree_handoff(source.path()).is_err());
    }

    fn git_output(cwd: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(args)
            .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false")
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    }

    fn run_head(cwd: &Path) -> String {
        String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false")
                .current_dir(cwd)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string()
    }

    #[test]
    fn merged_older_boundary_falls_back_to_snapshot_for_depth_one_receiver() {
        let (source, older_boundary) = fixture();
        git(source.path(), &["branch", "feature"]);
        std::fs::write(source.path().join("kept.txt"), "prerequisite\n").unwrap();
        git(source.path(), &["add", "."]);
        git(source.path(), &["commit", "-qm", "sandbox prerequisite"]);
        let prerequisite = run_head(source.path());
        let platform = tempfile::tempdir().unwrap();
        let origin_url = format!("file://{}", source.path().display());
        git(
            platform.path(),
            &["clone", "-q", "--depth=1", &origin_url, "repo"],
        );
        let restored = platform.path().join("repo");
        assert_eq!(run_head(&restored), prerequisite);
        assert_eq!(
            std::fs::read_to_string(restored.join(".git/shallow")).unwrap(),
            format!("{prerequisite}\n")
        );

        git(source.path(), &["checkout", "-q", "feature"]);
        std::fs::write(source.path().join("feature.txt"), "merged feature\n").unwrap();
        git(source.path(), &["add", "."]);
        git(
            source.path(),
            &["commit", "-qm", "feature from older boundary"],
        );
        let feature_head = run_head(source.path());
        git(
            source.path(),
            &["checkout", "--detach", "-q", &prerequisite],
        );
        git(
            source.path(),
            &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
        );
        let head = run_head(source.path());
        let tree = git_output(source.path(), &["rev-parse", "HEAD^{tree}"]);
        let snapshot = capture_worktree_handoff_cancellable(
            source.path(),
            Some(&prerequisite),
            &CancellationToken::new(),
        )
        .unwrap();
        let (unpacked, manifest) = unpack(&snapshot);
        assert_eq!(manifest["repository"]["shallow"], true);
        assert!(manifest["repository"].get("prerequisiteSha").is_none());
        source.close().unwrap();

        std::fs::write(
            restored.join(".git/shallow"),
            format!("{prerequisite}\n{head}\n"),
        )
        .unwrap();
        let bundle = unpacked.path().join(REPOSITORY_PATH);
        git(&restored, &["bundle", "verify", bundle.to_str().unwrap()]);
        git(&restored, &["bundle", "unbundle", bundle.to_str().unwrap()]);
        git(&restored, &["checkout", "--detach", "-q", &head]);
        git(&restored, &["fsck", "--strict"]);
        assert_eq!(run_head(&restored), head);
        assert_eq!(git_output(&restored, &["rev-parse", "HEAD^{tree}"]), tree);
        assert_eq!(
            git_output(&restored, &["rev-list", "--count", "HEAD"]),
            b"1\n"
        );
        assert_eq!(
            std::fs::read(restored.join("kept.txt")).unwrap(),
            b"prerequisite\n"
        );
        assert_eq!(
            std::fs::read(restored.join("feature.txt")).unwrap(),
            b"merged feature\n"
        );
        for unavailable in [&older_boundary, &feature_head] {
            assert!(
                !Command::new("git")
                    .args(["cat-file", "-e", unavailable])
                    .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false")
                    .current_dir(&restored)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
    }

    #[test]
    fn delta_restores_blob_from_before_shallow_prerequisite() {
        let (source, _) = fixture();
        let old_blob = git_output(source.path(), &["rev-parse", "HEAD:kept.txt"]);
        let old_blob = std::str::from_utf8(&old_blob).unwrap().trim();
        std::fs::write(source.path().join("kept.txt"), "replacement\n").unwrap();
        git(source.path(), &["commit", "-qam", "replace old blob"]);
        let prerequisite = run_head(source.path());
        let platform = tempfile::tempdir().unwrap();
        git(
            platform.path(),
            &[
                "clone",
                "-q",
                "--depth=1",
                &format!("file://{}", source.path().display()),
                "repo",
            ],
        );
        let restored = platform.path().join("repo");
        assert!(
            !Command::new("git")
                .args(["cat-file", "-e", old_blob])
                .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false")
                .current_dir(&restored)
                .output()
                .unwrap()
                .status
                .success()
        );
        std::fs::write(source.path().join("kept.txt"), "base\n").unwrap();
        git(
            source.path(),
            &["commit", "-qam", "restore historical blob"],
        );
        let head = run_head(source.path());
        let snapshot = capture_worktree_handoff_cancellable(
            source.path(),
            Some(&prerequisite),
            &CancellationToken::new(),
        )
        .unwrap();
        let (unpacked, manifest) = unpack(&snapshot);
        assert_eq!(manifest["repository"]["prerequisiteSha"], prerequisite);
        source.close().unwrap();
        let bundle = unpacked.path().join(REPOSITORY_PATH);
        git(&restored, &["bundle", "verify", bundle.to_str().unwrap()]);
        git(&restored, &["bundle", "unbundle", bundle.to_str().unwrap()]);
        git(&restored, &["checkout", "--detach", "-q", &head]);
        git(&restored, &["fsck", "--strict"]);
        assert_eq!(run_head(&restored), head);
        assert_eq!(std::fs::read(restored.join("kept.txt")).unwrap(), b"base\n");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fetch_memory_inspection_ignores_exited_unreaped_children() {
        use std::os::unix::process::CommandExt as _;
        let mut child = Command::new("git")
            .arg("--version")
            .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false")
            .stdout(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut status = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut status,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        let inspected = inspect_fetch_memory(child.id(), 0);
        child.wait().unwrap();
        assert_eq!(waited, 0);
        assert!(inspected.is_ok(), "{inspected:?}");
    }
}
