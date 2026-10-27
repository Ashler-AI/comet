use super::*;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const VERSION: &str = "crew.native-isolated-recovery.v1";
const MAX_JSON: u64 = 64 * 1024;

const PREPARE_UPLOAD_PYTHON: &str = r#"import os, pathlib, re, sys
platform = pathlib.Path(sys.argv[1])
namespace, attempt = sys.argv[2:]
if not platform.is_absolute() or platform.resolve(strict=True) != platform or re.fullmatch(r"[0-9a-f]{64}", namespace) is None or re.fullmatch(r"[0-9a-f]{32}", attempt) is None:
    raise SystemExit(1)
fd = os.open(platform / ".scaffold", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
try:
    for name in ("crew-native-" + namespace, "uploads", attempt):
        try: os.mkdir(name, 0o700, dir_fd=fd)
        except FileExistsError: pass
        child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
        try:
            os.fchmod(child, 0o700)
            os.fsync(child)
            os.fsync(fd)
        except BaseException:
            os.close(child)
            raise
        os.close(fd)
        fd = child
finally:
    os.close(fd)
"#;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Binding {
    version: String,
    provenance: String,
    operation_id: String,
    namespace: String,
    sandbox_id: String,
    scope: Value,
    environment: Value,
    source_selector: String,
    source_cwd: String,
    session_only: bool,
}
#[derive(Serialize, Deserialize)]
struct NativeArtifact {
    archive_sha256: String,
    archive_bytes: u64,
    storage_relative_path: String,
    sha256: String,
    byte_count: u64,
    native_session_id: String,
    cwd: String,
}

#[derive(Serialize, Deserialize)]
struct RepositoryArtifact {
    archive_sha256: String,
    archive_bytes: u64,
    manifest_sha256: String,
    base_sha: String,
    entry_count: usize,
    cwd_relative_path: String,
    bundle_sha256: String,
    bundle_bytes: u64,
    prerequisite_sha: Option<String>,
    shallow: Option<bool>,
}

#[derive(Serialize, Deserialize)]
struct Descriptor {
    #[serde(flatten)]
    binding: Binding,
    published_cwd: String,
    target_native_id: String,
    native: NativeArtifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository: Option<RepositoryArtifact>,
}

struct Capsule {
    directory: PathBuf,
    descriptor: Descriptor,
    bytes: Vec<u8>,
    sha256: String,
}

fn serialize_descriptor(descriptor: &Descriptor) -> Result<Vec<u8>, ScaffoldError> {
    struct JsonBytes(Vec<u8>);
    impl Write for JsonBytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_JSON as usize - self.0.len() {
                return Err(std::io::Error::other("recovery descriptor exceeds 64 KiB"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut output = JsonBytes(Vec::new());
    serde_json::to_writer(&mut output, descriptor).map_err(|error| ScaffoldError::InvalidResponse(format!("native recovery descriptor serialization: {error}")))?;
    Ok(output.0)
}

fn failure() -> ScaffoldError {
    ScaffoldError::HandoffStage("native_recovery_capsule")
}

fn recovery_diagnostic(binding: &Binding, descriptor_sha256: Option<&str>, mut artifacts: Value) -> IsolatedRecoveryDiagnostic {
    artifacts["target_native_id"] = isolated_recovery_native_id(&binding.operation_id).into();
    IsolatedRecoveryDiagnostic {
        operation_id: binding.operation_id.clone(), sandbox_id: binding.sandbox_id.clone(),
        descriptor_sha256: descriptor_sha256.map(str::to_owned), exit_code: None, stderr: String::new(), artifacts,
    }
}

fn diagnosed(stage: &'static str, mut diagnostic: IsolatedRecoveryDiagnostic, exit_code: Option<i32>, stderr: &str) -> ScaffoldError {
    let mut end = stderr.len().min(2048);
    while !stderr.is_char_boundary(end) { end -= 1; }
    diagnostic.exit_code = exit_code;
    diagnostic.stderr = stderr[..end].into();
    ScaffoldError::IsolatedRecovery { stage, diagnostic: Box::new(diagnostic) }
}

fn enrich(error: ScaffoldError, stage: &'static str, diagnostic: IsolatedRecoveryDiagnostic) -> ScaffoldError {
    match error {
        ScaffoldError::Cancelled | ScaffoldError::IsolatedRecovery { .. } => error,
        _ => diagnosed(stage, diagnostic, None, &error.to_string()),
    }
}

fn private_directory(path: &Path) -> Result<(), ScaffoldError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| capsule_io(path, "read directory", error))?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(capsule_io(path, "private directory validation", format!("expected directory mode 0700, observed mode {:o}", metadata.permissions().mode() & 0o777)));
    }
    Ok(())
}

fn open_directory(path: &Path) -> Result<File, ScaffoldError> {
    OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path).map_err(|error| capsule_io(path, "open directory without following symlinks", error))
}


fn clean_owned_capsule(operation_id: &str, capsule_root: &Path) -> Result<(), ScaffoldError> {
    if !operation_id.starts_with("crew-handoff:") || !operation_id.ends_with(":isolated-recovery") || operation_id.len() > 4096 {
        return Err(capsule_io(capsule_root, "cleanup operation validation", "expected an isolated recovery operation"));
    }
    match fs::symlink_metadata(capsule_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(capsule_io(capsule_root, "inspect cleanup root", error)),
        Ok(_) => {}
    }
    private_directory(capsule_root)?;
    let root_directory = open_directory(capsule_root)?;
    let directory = capsule_root.join(namespace(operation_id));
    match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return root_directory.sync_all().map_err(|error| capsule_io(capsule_root, "sync completed cleanup", error));
        }
        Err(error) => return Err(capsule_io(&directory, "inspect cleanup namespace", error)),
        Ok(_) => {}
    }
    private_directory(&directory)?;
    let _owned_directory = open_directory(&directory)?;
    // Admission is already durable. A previous cleanup may have removed any
    // subset of the capsule, so its descriptor and archives are not prerequisites.
    match fs::remove_dir_all(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(capsule_io(&directory, "remove completed capsule", error)),
    }
    root_directory.sync_all().map_err(|error| capsule_io(capsule_root, "sync completed cleanup", error))
}

fn open_private(path: &Path, limit: u64) -> Result<File, ScaffoldError> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path).map_err(|error| capsule_io(path, "open private artifact", error))?;
    let metadata = file.metadata().map_err(|error| capsule_io(path, "read artifact metadata", error))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o777 != 0o600 || metadata.len() > limit {
        return Err(capsule_io(path, "private artifact validation", format!("expected regular file mode 0600 and <= {limit} bytes, observed mode {:o}, {} bytes", metadata.permissions().mode() & 0o777, metadata.len())));
    }
    Ok(file)
}

fn read_private(path: &Path, limit: u64) -> Result<Vec<u8>, ScaffoldError> {
    let mut bytes = Vec::new();
    open_private(path, limit)?.take(limit + 1).read_to_end(&mut bytes).map_err(|error| capsule_io(path, "read private artifact", error))?;
    if bytes.len() as u64 > limit { return Err(failure()); }
    Ok(bytes)
}

fn stream_digest(mut source: File, mut output: Option<&mut File>, limit: u64, cancellation: &CancellationToken) -> Result<(String, u64), ScaffoldError> {
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancellation.is_cancelled() { return Err(ScaffoldError::Cancelled); }
        let count = source.read(&mut buffer).map_err(|error| ScaffoldError::InvalidResponse(format!("native recovery archive read: {error}")))?;
        if count == 0 { break; }
        bytes = bytes.checked_add(count as u64).filter(|bytes| *bytes <= limit).ok_or_else(failure)?;
        digest.update(&buffer[..count]);
        if let Some(output) = output.as_deref_mut() {
            output.write_all(&buffer[..count]).map_err(|error| ScaffoldError::InvalidResponse(format!("native recovery archive write: {error}")))?;
        }
    }
    Ok((format!("{:x}", digest.finalize()), bytes))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), ScaffoldError> {
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW).open(path).map_err(|_| failure())?;
    file.write_all(bytes).and_then(|_| file.sync_all()).map_err(|_| failure())
}

fn persist_archive(source: NamedTempFile, path: &Path, expected_bytes: u64, cancellation: &CancellationToken) -> Result<String, ScaffoldError> {
    if expected_bytes == 0 || expected_bytes > MAX_HANDOFF_ARCHIVE_BYTES { return Err(failure()); }
    let (digest, count) = stream_digest(source.reopen().map_err(|error| capsule_io(path, "open captured archive", error))?, None, expected_bytes, cancellation)?;
    if count != expected_bytes { return Err(capsule_io(path, "archive size validation", format!("expected {expected_bytes}, observed {count}"))); }
    source.as_file().set_permissions(fs::Permissions::from_mode(0o600))
        .and_then(|_| source.as_file().sync_all()).map_err(|error| capsule_io(path, "sync captured archive", error))?;
    match source.persist_noclobber(path) {
        Ok(_) => {}
        Err(error) if error.error.raw_os_error() == Some(libc::EXDEV) => {
            let source = error.file;
            let mut target = OpenOptions::new().write(true).create_new(true).mode(0o600)
                .custom_flags(libc::O_NOFOLLOW).open(path).map_err(|error| capsule_io(path, "create archive", error))?;
            let (copied_digest, count) = stream_digest(source.reopen().map_err(|error| capsule_io(path, "reopen captured archive", error))?, Some(&mut target), expected_bytes, cancellation)?;
            if count != expected_bytes || copied_digest != digest {
                return Err(capsule_io(path, "copied archive validation", format!("expected {digest}/{expected_bytes}, observed {copied_digest}/{count}")));
            }
            target.sync_all().map_err(|error| capsule_io(path, "sync archive", error))?;
        }
        Err(error) => return Err(capsule_io(path, "persist archive without replacement", error.error)),
    }
    Ok(digest)
}

fn capsule_io(path: &Path, action: &str, error: impl fmt::Display) -> ScaffoldError {
    ScaffoldError::InvalidResponse(format!("native recovery capsule {action} {}: {error}", path.display()))
}

fn load_capsule(directory: PathBuf, cancellation: &CancellationToken) -> Result<Capsule, ScaffoldError> {
    private_directory(&directory)?;
    let bytes = read_private(&directory.join("descriptor.json"), MAX_JSON)?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    if read_private(&directory.join("descriptor.sha256"), 64)? != sha256.as_bytes() {
        return Err(capsule_io(&directory.join("descriptor.json"), "descriptor checksum validation", "serialized descriptor changed"));
    }
    let descriptor: Descriptor = serde_json::from_slice(&bytes).map_err(|error| capsule_io(&directory.join("descriptor.json"), "decode descriptor", error))?;
    if descriptor.binding.version != VERSION || descriptor.binding.provenance != "legacy-recapture"
        || descriptor.binding.namespace != namespace(&descriptor.binding.operation_id)
        || directory.file_name().and_then(|name| name.to_str()) != Some(descriptor.binding.namespace.as_str())
        || descriptor.binding.session_only != descriptor.repository.is_none()
        || descriptor.target_native_id != isolated_recovery_native_id(&descriptor.binding.operation_id)
        || descriptor.target_native_id == descriptor.native.native_session_id
        || descriptor.published_cwd != published_cwd(&descriptor.binding, descriptor.repository.as_ref()) {
        return Err(capsule_io(&directory.join("descriptor.json"), "descriptor binding validation", "version, provenance, operation namespace, transfer mode or published cwd conflicts"));
    }
    let native = &descriptor.native;
    verify_archive(&directory.join("native.tar"), &native.archive_sha256, native.archive_bytes, cancellation)?;
    if let Some(repository) = &descriptor.repository {
        verify_archive(&directory.join("repository.tar"), &repository.archive_sha256, repository.archive_bytes, cancellation)?;
    }
    Ok(Capsule { directory, descriptor, bytes, sha256 })
}

fn verify_archive(path: &Path, digest: &str, bytes: u64, cancellation: &CancellationToken) -> Result<(), ScaffoldError> {
    if bytes == 0 || bytes > MAX_HANDOFF_ARCHIVE_BYTES { return Err(failure()); }
    let (actual_digest, actual_bytes) = stream_digest(open_private(path, bytes)?, None, bytes, cancellation)?;
    if actual_bytes != bytes || actual_digest != digest {
        return Err(capsule_io(path, "archive digest validation", format!("expected {digest}/{bytes}, observed {actual_digest}/{actual_bytes}")));
    }
    Ok(())
}

fn load_or_capture(
    root: &Path, binding: &Binding, cancellation: &CancellationToken,
    capture: impl FnOnce() -> Result<(OmpHandoffArchive, Option<WorktreeHandoffArchive>), ScaffoldError>,
) -> Result<Capsule, ScaffoldError> {
    let parent = root.parent().filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| capsule_io(root, "locate capsule parent", "capsule root requires an existing parent directory"))?;
    let parent_directory = open_directory(parent)?;
    match fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(capsule_io(root, "create capsule root", error)),
    }
    private_directory(root)?;
    let root_directory = open_directory(root)?;
    // Existing-root retries must also finish persisting an earlier mkdir.
    parent_directory.sync_all().map_err(|error| capsule_io(parent, "sync capsule parent", error))?;
    let directory = root.join(&binding.namespace);
    match fs::symlink_metadata(&directory) {
        Ok(_) => {
            let capsule = load_capsule(directory, cancellation)?;
            if &capsule.descriptor.binding != binding { return Err(failure()); }
            return Ok(capsule);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(failure()),
    }
    fs::DirBuilder::new().mode(0o700).create(&directory).map_err(|_| failure())?;
    root_directory.sync_all().map_err(|error| capsule_io(root, "sync capsule namespace", error))?;
    let (native, repository) = capture()?;
    if binding.session_only != repository.is_none() { return Err(failure()); }
    let mut artifacts = serde_json::json!({
        "native": {"archive_bytes": native.archive_byte_count, "storage_relative_path": &native.storage_relative_path,
            "sha256": &native.sha256, "byte_count": native.byte_count, "native_session_id": &native.native_session_id, "cwd": &native.cwd},
        "repository": repository.as_ref().map(|repository| serde_json::json!({
            "archive_bytes": repository.byte_count, "manifest_sha256": &repository.manifest_sha256, "base_sha": &repository.base_sha,
            "cwd_relative_path": &repository.cwd_relative_path, "entry_count": repository.entry_count,
            "bundle_sha256": &repository.repository.sha256, "bundle_bytes": repository.repository.byte_count,
            "prerequisite_sha": &repository.repository.prerequisite_sha, "shallow": repository.repository.shallow}))
    });
    let mut descriptor_sha256 = None;
    let result = (|| {
    let native_sha = persist_archive(native.file, &directory.join("native.tar"), native.archive_byte_count, cancellation)?;
    artifacts["native"]["archive_sha256"] = native_sha.clone().into();
    let native = NativeArtifact {
        archive_sha256: native_sha, archive_bytes: native.archive_byte_count,
        storage_relative_path: native.storage_relative_path, sha256: native.sha256,
        byte_count: native.byte_count, native_session_id: native.native_session_id, cwd: native.cwd,
    };
    let repository = repository.map(|mut repository| {
        let mut artifact = RepositoryArtifact { archive_sha256: String::new(), archive_bytes: repository.byte_count,
            manifest_sha256: std::mem::take(&mut repository.manifest_sha256), base_sha: std::mem::take(&mut repository.base_sha),
            entry_count: repository.entry_count, cwd_relative_path: std::mem::take(&mut repository.cwd_relative_path),
            bundle_sha256: std::mem::take(&mut repository.repository.sha256), bundle_bytes: repository.repository.byte_count,
            prerequisite_sha: repository.repository.prerequisite_sha.take(), shallow: repository.repository.shallow };
        artifact.archive_sha256 = persist_archive(repository.into_file(), &directory.join("repository.tar"), artifact.archive_bytes, cancellation)?;
        artifacts["repository"]["archive_sha256"] = artifact.archive_sha256.clone().into();
        Ok::<_, ScaffoldError>(artifact)
    }).transpose()?;
    let published_cwd = published_cwd(binding, repository.as_ref());
    let target_native_id = isolated_recovery_native_id(&binding.operation_id);
    if target_native_id == native.native_session_id {
        return Err(capsule_io(&directory, "target identity validation", "derived target must differ from saved source identity"));
    }
    let descriptor = Descriptor { binding: binding.clone(), published_cwd, target_native_id, native, repository };
    let bytes = serialize_descriptor(&descriptor)?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    descriptor_sha256 = Some(sha256.clone());
    write_private(&directory.join("descriptor.sha256"), sha256.as_bytes())?;
    File::open(&directory).and_then(|file| file.sync_all()).map_err(|error| capsule_io(&directory, "sync artifacts", error))?;
    let mut descriptor_file = NamedTempFile::new_in(&directory).map_err(|error| capsule_io(&directory, "create descriptor", error))?;
    descriptor_file.write_all(&bytes).and_then(|_| descriptor_file.as_file().sync_all())
        .map_err(|error| capsule_io(&directory, "sync descriptor", error))?;
    descriptor_file.persist_noclobber(directory.join("descriptor.json"))
        .map_err(|error| capsule_io(&directory, "publish descriptor without replacement", error.error))?;
    File::open(&directory).and_then(|file| file.sync_all()).map_err(|_| failure())?;
    File::open(root).and_then(|file| file.sync_all()).map_err(|_| failure())?;
    Ok(Capsule { directory, descriptor, bytes, sha256 })
    })();
    result.map_err(|error| enrich(error, "native_recovery_capsule", recovery_diagnostic(binding, descriptor_sha256.as_deref(), artifacts)))
}

impl ScaffoldRuntime {
    pub(crate) async fn recover_native_handoff(
        &self,
        operation_id: &str,
        environment: &SessionEnvironment,
        source_selector: &str,
        source_cwd: &str,
        session_only: bool,
        capsule_root: &Path,
        cancellation: &CancellationToken,
    ) -> Result<IsolatedRecoveryReceipt, ScaffoldError> {
        let mut diagnostic = IsolatedRecoveryDiagnostic {
            operation_id: operation_id.into(), sandbox_id: environment_sandbox_id(environment).unwrap_or_default().into(),
            descriptor_sha256: None, exit_code: None, stderr: String::new(),
            artifacts: serde_json::json!({"source_selector":source_selector,"source_cwd":source_cwd,"session_only":session_only,"target_native_id":isolated_recovery_native_id(operation_id)}),
        };
        let result = async {
        if !operation_id.starts_with("crew-handoff:") || !operation_id.ends_with(":isolated-recovery")
            || operation_id.len() > 4096 || !Path::new(source_selector).is_absolute()
            || !Path::new(source_cwd).is_absolute() || source_selector.len() > 4096 || source_cwd.len() > 4096
            || source_selector.chars().chain(source_cwd.chars()).any(char::is_control) {
            return Err(failure());
        }
        self.inner.client.validate_scope(&environment.scope)?;
        let (_, epoch, sandbox_id) = scaffold_source(environment)?;
        if epoch.is_none() || environment.database_environment.is_none() || environment.owner_principal.is_empty() {
            return Err(failure());
        }
        let handoff_permit = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ScaffoldError::Cancelled),
            permit = self.inner.handoff_permits.clone().acquire_owned() => permit.map_err(|_| failure())?,
        };
        let operation_namespace = namespace(operation_id);
        let binding = Binding {
            version: VERSION.into(), provenance: "legacy-recapture".into(),
            operation_id: operation_id.into(), namespace: operation_namespace,
            sandbox_id: sandbox_id.into(),
            scope: serde_json::to_value(&environment.scope).map_err(|_| failure())?,
            environment: serde_json::to_value(environment).map_err(|_| failure())?,
            source_selector: source_selector.into(), source_cwd: source_cwd.into(),
            session_only,
        };
        let root = capsule_root.to_path_buf();
        let capture_cancellation = cancellation.clone();
        let (_permit, capsule) = tokio::task::spawn_blocking(move || {
            let capsule = load_or_capture(&root, &binding, &capture_cancellation, || {
                let captured = crate::omp_session_artifact::capture_omp_file_for_session(
                    &binding.source_selector, &binding.source_cwd, &capture_cancellation)
                    .map_err(|error| if capture_cancellation.is_cancelled() { ScaffoldError::Cancelled } else {
                        diagnosed("native_session_capture", recovery_diagnostic(&binding, None,
                            serde_json::json!({"source_selector": &binding.source_selector, "source_cwd": &binding.source_cwd})), None, &error.to_string())
                    })?;
                let mut artifacts = serde_json::json!({"native": {"storage_relative_path": &captured.storage_relative_path,
                    "sha256": &captured.sha256, "byte_count": captured.byte_count,
                    "native_session_id": &captured.native_session_id, "cwd": &captured.cwd}});
                let native = prepare_omp_handoff_archive(captured, &capture_cancellation)
                    .map_err(|error| enrich(error, "native_archive_capture", recovery_diagnostic(&binding, None, artifacts.clone())))?;
                artifacts["native"]["archive_bytes"] = native.archive_byte_count.into();
                let repository = if binding.session_only { None } else {
                    Some(crate::worktree_handoff::capture_worktree_handoff_cancellable(
                        Path::new(&binding.source_cwd), None, &capture_cancellation)
                        .map_err(|error| if capture_cancellation.is_cancelled() { ScaffoldError::Cancelled } else {
                            diagnosed("source_repository_capture", recovery_diagnostic(&binding, None, artifacts.clone()), None, &error.to_string())
                        })?)
                };
                Ok((native, repository))
            })?;
            Ok::<_, ScaffoldError>((handoff_permit, capsule))
        }).await.map_err(|error| ScaffoldError::InvalidResponse(format!("native recovery capture task: {error}")))??;

        // The exact source capsule is durable before the first destination mutation.
        let current = self.inner.client.inspect(sandbox_id, &environment.scope, cancellation).await?;
        let (current_lifecycle, current_epoch, current_sandbox) = scaffold_source(&current)?;
        if current.scope != environment.scope || current.owner_principal != environment.owner_principal
            || current.database_environment != environment.database_environment || current_epoch != epoch
            || current_sandbox != sandbox_id
            || !matches!(current_lifecycle, zeron_proto::ScaffoldLifecycle::Ready | zeron_proto::ScaffoldLifecycle::AgentRunning) {
            return Err(failure());
        }
        let attempt_id = uuid::Uuid::new_v4().simple().to_string();
        let attempt = format!(".scaffold/crew-native-{}/uploads/{attempt_id}", capsule.descriptor.binding.namespace);
        let prepare_argv = vec!["python3".into(), "-c".into(), PREPARE_UPLOAD_PYTHON.into(),
            "/workspace/ashler-platform".into(), capsule.descriptor.binding.namespace.clone(), attempt_id];
        let prepared = self.inner.client.exec(sandbox_id,
            &ExecBody { argv: &prepare_argv, mode: "inline", timeout_ms: 10_000 }, cancellation).await?;
        if !prepared.ok || prepared.exit_code != Some(0) {
            return Err(diagnosed("native_recovery_upload_directory", recovery_diagnostic(&capsule.descriptor.binding, Some(&capsule.sha256),
                serde_json::json!({"native": &capsule.descriptor.native, "repository": &capsule.descriptor.repository})),
                prepared.exit_code, prepared.stderr.as_deref().unwrap_or_default()));
        }
        if let Some(repository) = &capsule.descriptor.repository {
            self.upload_recovery_archive(sandbox_id, &format!("{attempt}/repository"),
                &capsule.directory.join("repository.tar"), repository.archive_bytes, cancellation).await?;
        }
        self.upload_recovery_archive(sandbox_id, &format!("{attempt}/native"),
            &capsule.directory.join("native.tar"), capsule.descriptor.native.archive_bytes, cancellation).await?;
        let descriptor_path = format!("{attempt}/descriptor.json");
        self.inner.client.put_file(sandbox_id, &descriptor_path,
            std::str::from_utf8(&capsule.bytes).map_err(|_| failure())?, cancellation).await?;
        let helper_path = format!("{attempt}/recovery.py");
        let helper = format!("REPOSITORY_SOURCE = {}\nNATIVE_SOURCE = {}\n{}",
            serde_json::to_string(VERIFY_WORKTREE_HANDOFF_PYTHON).map_err(|_| failure())?,
            serde_json::to_string(VERIFY_HANDOFF_PYTHON).map_err(|_| failure())?,
            include_str!("../../isolated_recovery.py"));
        self.inner.client.put_file(sandbox_id, &helper_path, &helper, cancellation).await?;
        let argv = vec!["python3".into(), helper_path,
            format!("/workspace/ashler-platform/{descriptor_path}"), format!("/workspace/ashler-platform/{attempt}")];
        let result = self.inner.client.exec(sandbox_id,
            &ExecBody { argv: &argv, mode: "inline", timeout_ms: 60_000 }, cancellation).await?;
        if !result.ok || result.exit_code != Some(0) {
            return Err(diagnosed("native_recovery_guest", recovery_diagnostic(&capsule.descriptor.binding, Some(&capsule.sha256),
                serde_json::json!({"native": &capsule.descriptor.native, "repository": &capsule.descriptor.repository})),
                result.exit_code, result.stderr.as_deref().unwrap_or_default()));
        }
        let stdout = result.stdout.as_deref().ok_or_else(failure)?;
        if stdout.len() as u64 > MAX_JSON { return Err(failure()); }
        let receipt: IsolatedRecoveryReceipt = serde_json::from_str(stdout).map_err(|_| failure())?;
        validate_receipt(&capsule, &receipt)?;
        Ok(receipt)
        }.await;
        result.map_err(|error| {
            if matches!(error, ScaffoldError::Cancelled | ScaffoldError::IsolatedRecovery { .. }) { return error; }
            let directory = capsule_root.join(namespace(operation_id));
            if let Ok(bytes) = read_private(&directory.join("descriptor.json"), MAX_JSON) {
                let sha256 = format!("{:x}", Sha256::digest(&bytes));
                if read_private(&directory.join("descriptor.sha256"), 64).ok().as_deref() == Some(sha256.as_bytes()) {
                    if let Ok(descriptor) = serde_json::from_slice::<Descriptor>(&bytes) {
                        if descriptor.binding.operation_id == operation_id && descriptor.binding.sandbox_id == diagnostic.sandbox_id {
                            diagnostic.descriptor_sha256 = Some(sha256);
                            diagnostic.artifacts = serde_json::json!({"native": descriptor.native, "repository": descriptor.repository, "target_native_id":descriptor.target_native_id});
                        }
                    }
                }
            }
            enrich(error, "native_recovery", diagnostic)
        })
    }

    async fn upload_recovery_archive(&self, sandbox_id: &str, destination: &str, path: &Path,
        byte_count: u64, cancellation: &CancellationToken) -> Result<(), ScaffoldError> {
        let grant: UploadGrantEnvelope = self.inner.client.request(Method::POST,
            self.inner.client.sandbox_url(sandbox_id, Some("uploads"))?,
            Some(&UploadGrantRequest { destination_path: destination }), cancellation, None).await?;
        if !grant.ok || grant.upload.destination_path != destination { return Err(failure()); }
        validate_upload_grant(&grant.upload)?;
        let file = open_private(path, byte_count)?;
        self.inner.client.upload_granted_file(&grant.upload, file, byte_count, cancellation).await
    }

    // The caller invokes this only after durable original-command admission and receipt.
    pub(crate) fn cleanup_native_handoff_recovery(&self, operation_id: &str, capsule_root: &Path) -> Result<(), ScaffoldError> {
        clean_owned_capsule(operation_id, capsule_root)
    }
}

fn published_cwd(binding: &Binding, repository: Option<&RepositoryArtifact>) -> String {
    if binding.session_only { return "/workspace/ashler-platform".into(); }
    let root = format!("/workspace/crew-native-{}", binding.namespace);
    match repository.map(|repository| repository.cwd_relative_path.as_str()) {
        Some(relative) if !relative.is_empty() => format!("{root}/{relative}"),
        _ => root,
    }
}

fn validate_receipt(capsule: &Capsule, receipt: &IsolatedRecoveryReceipt) -> Result<(), ScaffoldError> {
    let descriptor = &capsule.descriptor;
    let cwd = &descriptor.published_cwd;
    let selector = Path::new(&receipt.launch_selector);
    let expected_name = format!("recovered-{}.jsonl", descriptor.binding.namespace);
    let encoded_cwd = format!("--{}--", cwd.trim_start_matches('/').replace(['/', '\\', ':'], "-"));
    if receipt.descriptor_sha256 != capsule.sha256 || receipt.cwd != *cwd
        || receipt.native_session_id != descriptor.target_native_id || receipt.native_session_id == descriptor.native.native_session_id
        || receipt.source_native_session_id != descriptor.native.native_session_id || receipt.source_sha256 != descriptor.native.sha256
        || receipt.rebound_sha256.len() != 64 || !receipt.rebound_sha256.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || receipt.rebound_bytes == 0 || receipt.rebound_bytes > descriptor.native.byte_count.saturating_add(MAX_JSON)
        || !selector.is_absolute()
        || receipt.launch_selector.len() > 8192
        || receipt.launch_selector.split('/').skip(1).any(|part| part.is_empty() || matches!(part, "." | ".."))
        || selector.components().any(|component| !matches!(component, std::path::Component::RootDir | std::path::Component::Normal(_)))
        || selector.file_name() != Some(std::ffi::OsStr::new(&expected_name))
        || selector.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new(&encoded_cwd))
        || selector.parent().and_then(Path::parent).and_then(Path::file_name) != Some(std::ffi::OsStr::new("sessions")) {
        return Err(ScaffoldError::HandoffStage("native_recovery_receipt"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git").arg("-C").arg(root)
            .args(args).env("GIT_CONFIG_NOSYSTEM", "1")
            .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    fn rebound_receipt(capsule: &Capsule) -> IsolatedRecoveryReceipt {
        let mut archive = File::open(capsule.directory.join("native.tar")).unwrap();
        std::io::Seek::seek(&mut archive, std::io::SeekFrom::Start(512)).unwrap();
        let mut source = Vec::new();
        archive.take(capsule.descriptor.native.byte_count).read_to_end(&mut source).unwrap();
        let newline = source.iter().position(|byte| *byte == b'\n').unwrap();
        let mut header: Value = serde_json::from_slice(&source[..newline]).unwrap();
        header["id"] = capsule.descriptor.target_native_id.clone().into();
        header["cwd"] = capsule.descriptor.published_cwd.clone().into();
        let mut rebound = serde_json::to_vec(&header).unwrap();
        rebound.extend_from_slice(&source[newline..]);
        let cwd = capsule.descriptor.published_cwd.clone();
        let encoded = format!("--{}--", cwd.trim_start_matches('/').replace(['/', '\\', ':'], "-"));
        IsolatedRecoveryReceipt {
            descriptor_sha256: capsule.sha256.clone(), cwd,
            native_session_id: capsule.descriptor.target_native_id.clone(),
            source_native_session_id: capsule.descriptor.native.native_session_id.clone(),
            source_sha256: capsule.descriptor.native.sha256.clone(),
            rebound_sha256: format!("{:x}", Sha256::digest(&rebound)), rebound_bytes: rebound.len() as u64,
            launch_selector: format!("/runtime/agent/sessions/{encoded}/recovered-{}.jsonl", capsule.descriptor.binding.namespace),
        }
    }

    fn fixture() -> (tempfile::TempDir, Binding, OmpHandoffArchive, WorktreeHandoffArchive) {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        std::fs::write(root.path().join("tracked"), "original\n").unwrap();
        git(root.path(), &["add", "."]);
        git(root.path(), &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture"]);
        std::fs::write(root.path().join("tracked"), "overlay\n").unwrap();
        let journal = tempfile::NamedTempFile::new().unwrap();
        let cwd = root.path().to_str().unwrap();
        let history = [
            serde_json::json!({"type":"session","id":"native-original","cwd":cwd}).to_string(),
            serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"complete history"},{"type":"image","data":"aGlzdG9yaWNhbC1hdHRhY2htZW50","mimeType":"image/png"}]}}).to_string(),
        ].join("\n") + "\n";
        std::fs::write(journal.path(), &history).unwrap();
        let captured = crate::omp_session_artifact::capture_omp_session_file(
            journal.path(), Path::new("crew-native/native-original.jsonl"), "native-original", cwd,
            &CancellationToken::new()).unwrap();
        let native = prepare_omp_handoff_archive(captured, &CancellationToken::new()).unwrap();
        assert_eq!(std::fs::read(journal.path()).unwrap(), history.as_bytes(), "original private fork must remain unchanged");
        let mut archive = native.reopen().unwrap();
        std::io::Seek::seek(&mut archive, std::io::SeekFrom::Start(512)).unwrap();
        let mut preserved = Vec::new();
        archive.take(native.byte_count).read_to_end(&mut preserved).unwrap();
        assert_eq!(preserved, history.as_bytes(), "all history and inline attachments must be byte-exact");
        let repository = crate::worktree_handoff::capture_worktree_handoff(root.path()).unwrap();
        let binding = Binding {
            version: VERSION.into(), provenance: "legacy-recapture".into(),
            operation_id: "crew-handoff:original:isolated-recovery".into(),
            namespace: namespace("crew-handoff:original:isolated-recovery"),
            sandbox_id: "sandbox-original".into(), scope: serde_json::json!({"projectId":"project"}),
            environment: serde_json::json!({"epoch":7,"database":"local"}),
            source_selector: journal.path().to_str().unwrap().into(), source_cwd: cwd.into(), session_only: false,
        };
        (root, binding, native, repository)
    }

    #[test]
    fn capsule_reuses_exact_captures_without_recapturing_changed_source() {
        let (source, binding, native, repository) = fixture();
        let private = tempfile::tempdir().unwrap();
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let capsule_root = private.path().join("native-handoff-recovery");
        let first = load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || Ok((native, Some(repository)))).unwrap();
        let original = std::fs::read(first.directory.join("repository.tar")).unwrap();
        let descriptor = first.bytes.clone();
        let digest = first.sha256.clone();
        drop(first);
        std::fs::write(source.path().join("tracked"), "later source changes\n").unwrap();
        let reused = load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || panic!("must never recapture")).unwrap();
        assert_eq!(descriptor, reused.bytes);
        assert_eq!(digest, reused.sha256);
        assert_eq!(std::fs::read(reused.directory.join("repository.tar")).unwrap(), original);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(private.path()).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(std::fs::metadata(&capsule_root).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&reused.directory).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(reused.directory.join("descriptor.json")).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(load_or_capture(private.path(), &binding, &CancellationToken::new(), || panic!("an existing mode-0755 capsule root must be rejected")).is_err());
        assert_eq!(std::fs::metadata(private.path()).unwrap().permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn capsule_rejects_conflicting_binding_missing_and_modified_artifacts() {
        for scenario in ["selector", "cwd", "session_only", "epoch", "scope", "database", "missing-native", "missing-repository", "missing-descriptor", "missing-digest", "modified-native", "modified-repository", "modified-descriptor", "native-mode"] {
            let (_source, binding, native, repository) = fixture();
            let private = tempfile::tempdir().unwrap();
            let capsule_root = private.path().join("native-handoff-recovery");
            let first = load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || Ok((native, Some(repository)))).unwrap();
            let mut retry = binding.clone();
            match scenario {
                "selector" => retry.source_selector.push_str("-changed"),
                "cwd" => retry.source_cwd.push_str("/changed"),
                "session_only" => retry.session_only = true,
                "epoch" => retry.environment["epoch"] = 8.into(),
                "scope" => retry.scope["projectId"] = "other".into(),
                "database" => retry.environment["database"] = "staging".into(),
                "missing-native" => std::fs::remove_file(first.directory.join("native.tar")).unwrap(),
                "missing-repository" => std::fs::remove_file(first.directory.join("repository.tar")).unwrap(),
                "missing-descriptor" => std::fs::remove_file(first.directory.join("descriptor.json")).unwrap(),
                "missing-digest" => std::fs::remove_file(first.directory.join("descriptor.sha256")).unwrap(),
                "modified-native" => std::fs::write(first.directory.join("native.tar"), b"changed").unwrap(),
                "modified-repository" => std::fs::write(first.directory.join("repository.tar"), b"changed").unwrap(),
                "modified-descriptor" => std::fs::write(first.directory.join("descriptor.json"), b"{}").unwrap(),
                "native-mode" => std::fs::set_permissions(first.directory.join("native.tar"), fs::Permissions::from_mode(0o644)).unwrap(),
                _ => unreachable!(),
            }
            assert!(load_or_capture(&capsule_root, &retry, &CancellationToken::new(), || panic!("must never recapture {scenario}")).is_err(), "{scenario}");
        }
    }

    #[test]
    fn incomplete_first_capture_never_recaptures() {
        let (_source, binding, native, repository) = fixture();
        let private = tempfile::tempdir().unwrap();
        let capsule_root = private.path().join("native-handoff-recovery");
        let result = load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || {
            drop((native, repository));
            Err(failure())
        });
        assert!(result.is_err());
        assert!(load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || panic!("incomplete capture cannot be retried")).is_err());
    }

    #[test]
    fn session_only_capsule_keeps_published_checkout_and_owned_journal_selector() {
        let (_source, mut binding, native, repository) = fixture();
        binding.session_only = true;
        drop(repository);
        let private = tempfile::tempdir().unwrap();
        let capsule_root = private.path().join("native-handoff-recovery");
        let capsule = load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || Ok((native, None))).unwrap();
        assert!(capsule.descriptor.repository.is_none());
        assert!(!capsule.directory.join("repository.tar").exists());
        assert_eq!(capsule.descriptor.published_cwd, "/workspace/ashler-platform");
        let mut receipt = rebound_receipt(&capsule);
        validate_receipt(&capsule, &receipt).unwrap();
        receipt.launch_selector = "/runtime/agent/sessions/--workspace-ashler-platform--/native-original.jsonl".into();
        assert!(validate_receipt(&capsule, &receipt).is_err());
        receipt.launch_selector = "native-original".into();
        assert!(validate_receipt(&capsule, &receipt).is_err());
    }

    #[test]
    fn descriptor_and_stream_bounds_reject_without_unbounded_copying() {
        let (_source, binding, native, repository) = fixture();
        let private = tempfile::tempdir().unwrap();
        let capsule_root = private.path().join("native-handoff-recovery");
        let capsule = load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || Ok((native, Some(repository)))).unwrap();
        let mut descriptor = capsule.descriptor;
        descriptor.binding.environment = serde_json::json!({"oversized":"x".repeat(MAX_JSON as usize)});
        assert!(serialize_descriptor(&descriptor).is_err());
        let source = tempfile::NamedTempFile::new().unwrap();
        fs::write(source.path(), b"ab").unwrap();
        assert!(stream_digest(source.reopen().unwrap(), None, 1, &CancellationToken::new()).is_err());
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(stream_digest(source.reopen().unwrap(), None, 2, &cancellation), Err(ScaffoldError::Cancelled)));
    }

    #[test]
    fn worktree_receipt_rejects_wrong_identity_cwd_and_namespace() {
        let (_source, binding, native, repository) = fixture();
        let private = tempfile::tempdir().unwrap();
        let capsule_root = private.path().join("native-handoff-recovery");
        let capsule = load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || Ok((native, Some(repository)))).unwrap();
        let cwd = format!("/workspace/crew-native-{}", binding.namespace);
        assert_eq!(capsule.descriptor.published_cwd, cwd);
        let receipt = rebound_receipt(&capsule);
        validate_receipt(&capsule, &receipt).unwrap();
        for scenario in ["descriptor", "cwd", "native", "legacy-cwd", "other-basename", "dot-path"] {
            let mut changed = receipt.clone();
            match scenario {
                "descriptor" => changed.descriptor_sha256 = "0".repeat(64),
                "cwd" => changed.cwd = "/workspace/crew-handoff".into(),
                "native" => changed.native_session_id = "another-native".into(),
                "legacy-cwd" => changed.launch_selector = format!("/runtime/agent/sessions/--workspace-crew-handoff--/recovered-{}.jsonl", binding.namespace),
                "other-basename" => changed.launch_selector = changed.launch_selector.replace("recovered-", "legacy-"),
                "dot-path" => changed.launch_selector = changed.launch_selector.replace("/sessions/", "/./sessions/"),
                _ => unreachable!(),
            }
            assert!(validate_receipt(&capsule, &changed).is_err(), "{scenario}");
        }
    }

    #[test]
    fn archive_persistence_moves_capture_inode_and_never_clobbers() {
        use std::os::unix::fs::MetadataExt;
        let (_source, _binding, native, _repository) = fixture();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.tar");
        let original = native.file.as_file().metadata().unwrap();
        let old_path = native.file.path().to_path_buf();
        let digest = persist_archive(native.file, &path, native.archive_byte_count, &CancellationToken::new()).unwrap();
        let persisted = fs::metadata(&path).unwrap();
        if original.dev() == persisted.dev() { assert_eq!(original.ino(), persisted.ino()); }
        assert!(!old_path.exists());
        assert_eq!(digest, stream_digest(File::open(&path).unwrap(), None, native.archive_byte_count, &CancellationToken::new()).unwrap().0);
        let before = fs::read(&path).unwrap();
        let conflicting = tempfile::NamedTempFile::new().unwrap();
        fs::write(conflicting.path(), b"different").unwrap();
        assert!(persist_archive(conflicting, &path, 9, &CancellationToken::new()).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn recovery_diagnostics_keep_exact_metadata_and_bounded_utf8_cause() {
        let (_source, binding, native, repository) = fixture();
        let private = tempfile::tempdir().unwrap();
        let capsule_root = private.path().join("native-handoff-recovery");
        let capsule = load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || Ok((native, Some(repository)))).unwrap();
        let artifacts = serde_json::json!({"native": &capsule.descriptor.native, "repository": &capsule.descriptor.repository});
        let error = diagnosed("native_recovery_guest", recovery_diagnostic(&binding, Some(&capsule.sha256), artifacts), Some(45), &"repository object expansion failed 🛑".repeat(200));
        let ScaffoldError::IsolatedRecovery { stage, diagnostic } = error else { panic!("structured recovery diagnostic required") };
        assert_eq!(stage, "native_recovery_guest");
        assert_eq!(diagnostic.operation_id, binding.operation_id);
        assert_eq!(diagnostic.sandbox_id, binding.sandbox_id);
        assert_eq!(diagnostic.descriptor_sha256.as_deref(), Some(capsule.sha256.as_str()));
        assert_eq!(diagnostic.exit_code, Some(45));
        assert!(diagnostic.stderr.len() <= 2048);
        assert!(diagnostic.stderr.starts_with("repository object expansion failed"));
        assert_eq!(diagnostic.artifacts["native"]["sha256"], capsule.descriptor.native.sha256);
        assert_eq!(diagnostic.artifacts["target_native_id"], capsule.descriptor.target_native_id);
        assert_eq!(diagnostic.artifacts["repository"]["bundle_sha256"], capsule.descriptor.repository.unwrap().bundle_sha256);
    }

    #[test]
    fn upload_preparation_is_private_and_rejects_symlink_ancestors() {
        let root = tempfile::tempdir().unwrap();
        let platform = root.path().canonicalize().unwrap();
        fs::create_dir(platform.join(".scaffold")).unwrap();
        let namespace = "a".repeat(64);
        let attempt = "b".repeat(32);
        let prepare = || std::process::Command::new("python3")
            .args(["-c", PREPARE_UPLOAD_PYTHON]).arg(&platform).args([&namespace, &attempt]).output().unwrap();
        assert!(prepare().status.success());
        let operation = platform.join(".scaffold").join(format!("crew-native-{namespace}"));
        for directory in [&operation, &operation.join("uploads"), &operation.join("uploads").join(&attempt)] {
            assert_eq!(fs::metadata(directory).unwrap().permissions().mode() & 0o777, 0o700);
        }
        fs::remove_dir_all(operation.join("uploads")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), operation.join("uploads")).unwrap();
        assert!(!prepare().status.success());
        assert!(!outside.path().join(&attempt).exists());
    }

    #[test]
    fn derived_identity_and_receipt_preserve_exact_source_provenance() {
        let (_source, binding, native, repository) = fixture();
        let private = tempfile::tempdir().unwrap();
        let capsule_root = private.path().join("native-handoff-recovery");
        let capsule = load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || Ok((native, Some(repository)))).unwrap();
        let target = isolated_recovery_native_id(&binding.operation_id);
        assert_eq!(capsule.descriptor.target_native_id, target);
        assert_ne!(target, capsule.descriptor.native.native_session_id);
        assert_eq!(isolated_recovery_native_id(&binding.operation_id), target);
        assert_ne!(isolated_recovery_native_id("crew-handoff:another:isolated-recovery"), target);
        let uuid = uuid::Uuid::parse_str(&target).unwrap();
        assert_eq!(uuid.as_bytes()[6] >> 4, 8);
        assert_eq!(uuid.as_bytes()[8] >> 6, 2);
        let receipt = rebound_receipt(&capsule);
        validate_receipt(&capsule, &receipt).unwrap();
        assert_ne!(receipt.rebound_sha256, receipt.source_sha256);
        for scenario in ["source-id", "source-sha", "target-a", "rebound-sha", "rebound-empty", "rebound-expanded"] {
            let mut changed = receipt.clone();
            match scenario {
                "source-id" => changed.source_native_session_id = "different-original".into(),
                "source-sha" => changed.source_sha256 = "0".repeat(64),
                "target-a" => changed.native_session_id = capsule.descriptor.native.native_session_id.clone(),
                "rebound-sha" => changed.rebound_sha256 = "A".repeat(64),
                "rebound-empty" => changed.rebound_bytes = 0,
                "rebound-expanded" => changed.rebound_bytes = capsule.descriptor.native.byte_count + MAX_JSON + 1,
                _ => unreachable!(),
            }
            assert!(validate_receipt(&capsule, &changed).is_err(), "{scenario}");
        }
        let mut descriptor = capsule.descriptor;
        descriptor.target_native_id = descriptor.native.native_session_id.clone();
        let bytes = serialize_descriptor(&descriptor).unwrap();
        fs::write(capsule.directory.join("descriptor.json"), &bytes).unwrap();
        fs::write(capsule.directory.join("descriptor.sha256"), format!("{:x}", Sha256::digest(&bytes))).unwrap();
        assert!(load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || panic!("invalid target cannot recapture A")).is_err());
    }


    #[test]
    fn capsule_parent_symlink_is_rejected_before_root_creation() {
        let (_source, binding, _native, _repository) = fixture();
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent-link");
        std::os::unix::fs::symlink(outside.path(), &parent).unwrap();
        assert!(load_or_capture(&parent.join("native-handoff-recovery"), &binding, &CancellationToken::new(), || panic!("symlink parent cannot capture")).is_err());
        assert!(!outside.path().join("native-handoff-recovery").exists());
    }

    #[test]
    fn completed_cleanup_resumes_partial_deletion_without_touching_other_data() {
        let (source, binding, native, repository) = fixture();
        let root = tempfile::tempdir().unwrap();
        let capsule_root = root.path().join("native-handoff-recovery");
        fs::DirBuilder::new().mode(0o700).create(&capsule_root).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), b"untouched").unwrap();
        let sibling = capsule_root.join(namespace("crew-handoff:other:isolated-recovery"));
        fs::DirBuilder::new().mode(0o700).create(&sibling).unwrap();
        fs::write(sibling.join("sentinel"), b"other capsule").unwrap();
        let capsule = load_or_capture(&capsule_root, &binding, &CancellationToken::new(), || Ok((native, Some(repository)))).unwrap();
        fs::remove_file(capsule.directory.join("native.tar")).unwrap();
        fs::remove_file(capsule.directory.join("descriptor.json")).unwrap();
        std::os::unix::fs::symlink(outside.path(), capsule.directory.join("outside-link")).unwrap();
        clean_owned_capsule(&binding.operation_id, &capsule_root).unwrap();
        clean_owned_capsule(&binding.operation_id, &capsule_root).unwrap();
        assert!(!capsule.directory.exists());
        assert!(capsule_root.is_dir());
        assert_eq!(fs::metadata(&capsule_root).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::read(sibling.join("sentinel")).unwrap(), b"other capsule");
        assert_eq!(fs::read(outside.path().join("sentinel")).unwrap(), b"untouched");
        assert_eq!(fs::read(source.path().join("tracked")).unwrap(), b"overlay\n");
    }

    #[test]
    fn completed_cleanup_rejects_symlinked_owned_root_or_namespace() {
        let operation = "crew-handoff:completed:isolated-recovery";
        let root = tempfile::tempdir().unwrap();
        let capsule_root = root.path().join("native-handoff-recovery");
        fs::DirBuilder::new().mode(0o700).create(&capsule_root).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), b"untouched").unwrap();
        let namespace_path = capsule_root.join(namespace(operation));
        std::os::unix::fs::symlink(outside.path(), &namespace_path).unwrap();
        assert!(clean_owned_capsule(operation, &capsule_root).is_err());
        assert!(namespace_path.is_symlink());
        let linked_root = root.path().join("root-link");
        std::os::unix::fs::symlink(outside.path(), &linked_root).unwrap();
        assert!(clean_owned_capsule(operation, &linked_root).is_err());
        assert_eq!(fs::read(outside.path().join("sentinel")).unwrap(), b"untouched");
    }
}
