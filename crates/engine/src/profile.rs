//! Workspace profile identity and storage boundaries.
//!
//! Stores, journals, and uploads are profile-scoped. Repositories, worktrees,
//! agent credentials, UI settings, and the device id remain device-scoped under
//! the engine data directory.
//!
//! Verified sandbox storage is session-scoped, not device/epoch-scoped. A durable
//! binding under `profiles/scaffold` retains the current authority's legacy root
//! in place (including absolute upload paths); authentication keeps its own key.
//! Both the binding and its directory must travel with a checkpoint. Older-epoch
//! legacy roots without that prior verified binding require explicit recovery;
//! they cannot be identified from a new credential and are never scanned/imported.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeron_proto::WorkspaceScope;

use crate::EngineError;

const LOCAL_PROFILE_FILE: &str = "local-profile.json";
const LOCAL_ORG_ID: &str = "local";
const LEGACY_UPLOADS_DIR: &str = "uploads";
const LEGACY_UPLOADS_OWNER_FILE: &str = "legacy-uploads-owner.json";
const MAX_SCAFFOLD_STORE_BINDING_BYTES: u64 = 4096;

/// A resolved, immutable identity and storage boundary for one engine runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineProfile {
    scope: WorkspaceScope,
    device_root: PathBuf,
    store_root: PathBuf,
    uploads_root: PathBuf,
    org_id: String,
    user_id: String,
    managed_scaffold: bool,
    device_id_override: Option<String>,
    scaffold_profile_key: Option<String>,
}

impl EngineProfile {
    /// Load or create the installation's stable local identity.
    pub fn local(data_dir: &Path) -> Result<Self, EngineError> {
        let profile_id = load_or_create_local_profile_id(data_dir)?;
        let store_root = data_dir.join("profiles").join("local");
        Ok(Self {
            scope: WorkspaceScope::Local,
            device_root: data_dir.to_path_buf(),
            uploads_root: store_root.join("uploads"),
            store_root,
            org_id: LOCAL_ORG_ID.to_string(),
            user_id: profile_id,
            managed_scaffold: false,
            device_id_override: None,
            scaffold_profile_key: None,
        })
    }

    /// Resolve an authenticated profile under its account-specific storage root.
    pub fn synced(data_dir: &Path, org_id: &str, user_id: &str) -> Self {
        Self::account_scoped(data_dir, org_id, user_id, WorkspaceScope::Synced)
    }

    /// Open only the verified Crew identity, isolated by both configured origins.
    /// The resulting paths never change if the live auth service changes accounts.
    pub fn scaffold(data_dir: &Path, auth: &crate::auth::Auth) -> Result<Self, EngineError> {
        let mut session = auth.verified_scaffold_session().ok_or_else(|| EngineError::Other(
            if auth.device_mode() {
                "Crew sandbox authentication failed; inspect the device bootstrap exchange error and recover this same sandbox. Local-profile fallback is disabled."
            } else {
                "Crew profile requires a verified identity"
            }.into()
        ))?;
        let key = zeron_sync::scaffold_auth::profile_key(&session);
        let store_root = scaffold_store_root(data_dir, &mut session, &key)?;
        Ok(Self {
            scope: WorkspaceScope::Synced,
            device_root: data_dir.to_path_buf(),
            uploads_root: store_root.join("uploads"),
            store_root,
            org_id: session.identity.project_scope,
            user_id: session.identity.user_id,
            managed_scaffold: true,
            device_id_override: session.device.map(|device| device.device_id),
            scaffold_profile_key: Some(key),
        })
    }

    /// Resolve the explicit development profile under its account-specific storage root.
    pub fn development(data_dir: &Path, org_id: &str, user_id: &str) -> Self {
        Self::account_scoped(data_dir, org_id, user_id, WorkspaceScope::Development)
    }

    fn account_scoped(data_dir: &Path, org_id: &str, user_id: &str, scope: WorkspaceScope) -> Self {
        let store_root = data_dir
            .join("orgs")
            .join(sanitize_path_id(org_id))
            .join(sanitize_path_id(user_id));
        Self {
            scope,
            device_root: data_dir.to_path_buf(),
            uploads_root: store_root.join("uploads"),
            store_root,
            org_id: org_id.to_string(),
            user_id: user_id.to_string(),
            managed_scaffold: false,
            device_id_override: None,
            scaffold_profile_key: None,
        }
    }

    pub fn managed_scaffold(&self) -> bool { self.managed_scaffold }

    pub fn device_id_override(&self) -> Option<&str> { self.device_id_override.as_deref() }

    pub fn scaffold_profile_key(&self) -> Option<&str> { self.scaffold_profile_key.as_deref() }

    pub fn scope(&self) -> WorkspaceScope {
        self.scope
    }

    pub fn device_root(&self) -> &Path {
        &self.device_root
    }

    pub fn store_root(&self) -> &Path {
        &self.store_root
    }

    pub fn uploads_root(&self) -> &Path {
        &self.uploads_root
    }

    pub fn org_id(&self) -> &str {
        &self.org_id
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// Claim the historical device-global uploads cache for one account.
    ///
    /// Legacy transcript entries persist absolute paths under `{data_dir}/uploads`,
    /// so moving the directory would break them. The first account opened after
    /// upgrade becomes its durable owner and may read it as a compatibility
    /// fallback. New writes always use [`Self::uploads_root`], and other accounts
    /// never receive the fallback root.
    pub(crate) fn claim_legacy_uploads_root(&self) -> Result<Option<PathBuf>, EngineError> {
        // Managed profiles require explicit import; never claim unscoped old data.
        if self.store_root.starts_with(self.device_root.join("profiles").join("scaffold")) {
            return Ok(None);
        }
        if self.scope == WorkspaceScope::Local {
            return Ok(None);
        }

        let legacy_root = self.device_root.join(LEGACY_UPLOADS_DIR);
        let metadata = match std::fs::metadata(&legacy_root) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if !metadata.is_dir() {
            return Err(EngineError::Other(format!(
                "invalid legacy uploads cache {}: expected a directory",
                legacy_root.display()
            )));
        }

        let owner_path = self.device_root.join(LEGACY_UPLOADS_OWNER_FILE);
        let expected = LegacyUploadsOwner {
            org_id: self.org_id.clone(),
            user_id: self.user_id.clone(),
        };
        let owner = match read_legacy_uploads_owner(&owner_path) {
            Ok(owner) => owner,
            Err(ProfileReadError::Missing) => {
                publish_legacy_uploads_owner(&owner_path, &expected)?;
                read_legacy_uploads_owner(&owner_path).map_err(ProfileReadError::into_engine)?
            }
            Err(ProfileReadError::Engine(err)) => return Err(err),
        };

        Ok((owner == expected).then_some(legacy_root))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScaffoldStoreBinding {
    scope_key: String,
    legacy_device_id: Option<String>,
}

/// Pure destination derivation for authenticated runtime and fenced offline
/// import. The caller supplies verified bindings; this path grants no authority.
#[allow(clippy::too_many_arguments)]
pub fn scaffold_profile_directory(
    data_dir: &Path, resource: &str, edge_origin: &str, user_id: &str, project_id: &str,
    deployment_id: Option<&str>, device_deployment_id: &str, session_id: &str, sandbox_id: &str,
) -> PathBuf {
    data_dir.join("profiles").join("scaffold").join(zeron_sync::scaffold_auth::sandbox_storage_key(
        resource, edge_origin, user_id, project_id, deployment_id, device_deployment_id, session_id, sandbox_id,
    ))
}


/// Resolve only verified authority. Never use this storage binding for tokens.
fn scaffold_store_root(
    data_dir: &Path,
    session: &mut zeron_sync::scaffold_auth::ScaffoldSession,
    auth_key: &str,
) -> Result<PathBuf, EngineError> {
    let profiles = data_dir.join("profiles").join("scaffold");
    let legacy = profiles.join(auth_key);
    let Some(device) = &session.device else { return Ok(legacy); };
    let stable = scaffold_profile_directory(
        data_dir, &session.resource, &session.edge_origin, &session.identity.user_id,
        &session.identity.project_scope, session.identity.deployment_id.as_deref(),
        &device.deployment_id, &device.session_id, &device.sandbox_id,
    );
    let scope_key = stable.file_name().and_then(|name| name.to_str()).expect("storage key is hex").to_owned();
    let binding_path = profiles.join(format!("{scope_key}.store.json"));
    std::fs::create_dir_all(data_dir)?;
    for parent in [data_dir.join("profiles"), profiles.clone()] {
        if !scaffold_store_exists(&parent)? {
            match std::fs::create_dir(&parent) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && scaffold_store_exists(&parent)? => {}
                Err(err) => return Err(err.into()),
            }
        }
    }
    let binding_exists = match std::fs::symlink_metadata(&binding_path) {
        Ok(metadata) if metadata.is_file() => true,
        Ok(_) => return Err(EngineError::Other(format!("invalid sandbox storage binding {}: expected a regular file", binding_path.display()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };
    if !binding_exists {
        let legacy_exists = scaffold_store_exists(&legacy)?;
        let stable_exists = scaffold_store_exists(&stable)?;
        if legacy_exists && stable_exists {
            return Err(scaffold_store_conflict(&legacy, &stable));
        }
        let binding = ScaffoldStoreBinding {
            scope_key: scope_key.clone(),
            legacy_device_id: legacy_exists.then(|| device.device_id.clone()),
        };
        if !legacy_exists && !stable_exists {
            tracing::info!("Creating sandbox storage for the verified session; earlier-epoch legacy data requires its prior verified storage binding and is not automatically imported");
            std::fs::create_dir_all(&stable)?;
        }
        // Publish complete bytes without replacing a concurrent resolver's choice.
        let mut temporary = tempfile::NamedTempFile::new_in(&profiles)?;
        serde_json::to_writer(temporary.as_file_mut(), &binding)
            .map_err(|err| EngineError::Other(format!("serialize sandbox storage binding: {err}")))?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&binding_path) {
            Ok(_) => {}
            Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.error.into()),
        }
    }
    let mut binding = read_scaffold_store_binding(&binding_path)?;
    if binding.scope_key != scope_key || binding.legacy_device_id.as_ref().is_some_and(|id| id.trim().is_empty()) {
        return Err(EngineError::Other(format!("invalid sandbox storage binding {}", binding_path.display())));
    }
    let root = if let Some(anchor) = &mut binding.legacy_device_id {
        // Recompute the legacy key from CURRENT verified scope plus the original
        // device anchor. A sidecar cannot select an arbitrary foreign directory.
        // This is only our local session snapshot, never live auth/credentials.
        std::mem::swap(&mut session.device.as_mut().expect("device profile").device_id, anchor);
        let legacy_key = zeron_sync::scaffold_auth::profile_key(session);
        std::mem::swap(&mut session.device.as_mut().expect("device profile").device_id, anchor);
        profiles.join(legacy_key)
    } else {
        stable.clone()
    };
    for other in [&legacy, &stable] {
        if other != &root && scaffold_store_exists(other)? {
            return Err(scaffold_store_conflict(&root, other));
        }
    }
    if !scaffold_store_exists(&root)? {
        return Err(EngineError::Other(format!(
            "sandbox storage binding requires missing directory {}; restore the coherent profile checkpoint before opening this session",
            root.display()
        )));
    }
    Ok(root)
}

fn read_scaffold_store_binding(path: &Path) -> Result<ScaffoldStoreBinding, EngineError> {
    let invalid = || EngineError::Other(format!("invalid sandbox storage binding {}: expected a regular, non-symlink JSON file of at most {MAX_SCAFFOLD_STORE_BINDING_BYTES} bytes", path.display()));
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, SECURITY_IDENTIFICATION};
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ)
            .security_qos_flags(SECURITY_IDENTIFICATION);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_TYPE_DISK, GetFileType};
        // SAFETY: the handle belongs to the live File and is only queried.
        if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(invalid());
        }
    }
    if !metadata.is_file() || metadata.len() > MAX_SCAFFOLD_STORE_BINDING_BYTES {
        return Err(invalid());
    }
    // Also bound the actual read: metadata cannot prevent concurrent growth.
    let mut bytes = Vec::with_capacity((MAX_SCAFFOLD_STORE_BINDING_BYTES + 1) as usize);
    file.take(MAX_SCAFFOLD_STORE_BINDING_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SCAFFOLD_STORE_BINDING_BYTES {
        return Err(invalid());
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}

fn scaffold_store_exists(path: &Path) -> Result<bool, EngineError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(EngineError::Other(format!("invalid sandbox store {}: expected a directory, not a link or file", path.display()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn scaffold_store_conflict(first: &Path, second: &Path) -> EngineError {
    EngineError::Other(format!(
        "conflicting sandbox stores {} and {}; explicit recovery is required, neither directory was merged or replaced",
        first.display(), second.display()
    ))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalProfileFile {
    id: Uuid,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyUploadsOwner {
    org_id: String,
    user_id: String,
}

fn load_or_create_local_profile_id(data_dir: &Path) -> Result<String, EngineError> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(LOCAL_PROFILE_FILE);
    match read_local_profile_id(&path) {
        Ok(id) => return Ok(id),
        Err(ProfileReadError::Missing) => {}
        Err(ProfileReadError::Engine(err)) => return Err(err),
    }

    let id = Uuid::new_v4();
    let mut bytes = serde_json::to_vec_pretty(&LocalProfileFile { id })
        .map_err(|err| EngineError::Other(format!("serialize local profile: {err}")))?;
    bytes.push(b'\n');

    // Publish a fully-written same-directory file without replacing a profile
    // another process may have created concurrently.
    let temp_path = data_dir.join(format!(
        ".{LOCAL_PROFILE_FILE}.tmp-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let write_result = (|| -> Result<(), EngineError> {
        let mut temp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        temp.write_all(&bytes)?;
        temp.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }

    let publish_result = std::fs::hard_link(&temp_path, &path);
    let _ = std::fs::remove_file(&temp_path);
    match publish_result {
        Ok(()) => Ok(id.to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            read_local_profile_id(&path).map_err(ProfileReadError::into_engine)
        }
        Err(err) => Err(err.into()),
    }
}

enum ProfileReadError {
    Missing,
    Engine(EngineError),
}

impl ProfileReadError {
    fn into_engine(self) -> EngineError {
        match self {
            Self::Missing => EngineError::Other("local profile disappeared during creation".into()),
            Self::Engine(err) => err,
        }
    }
}

fn read_local_profile_id(path: &Path) -> Result<String, ProfileReadError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProfileReadError::Missing);
        }
        Err(err) => return Err(ProfileReadError::Engine(err.into())),
    };
    let profile: LocalProfileFile = serde_json::from_slice(&bytes).map_err(|err| {
        ProfileReadError::Engine(EngineError::Other(format!(
            "invalid local profile {}: {err}",
            path.display()
        )))
    })?;
    if profile.id.is_nil() {
        return Err(ProfileReadError::Engine(EngineError::Other(format!(
            "invalid local profile {}: id must not be nil",
            path.display()
        ))));
    }
    Ok(profile.id.to_string())
}

fn read_legacy_uploads_owner(path: &Path) -> Result<LegacyUploadsOwner, ProfileReadError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProfileReadError::Missing);
        }
        Err(err) => return Err(ProfileReadError::Engine(err.into())),
    };
    serde_json::from_slice(&bytes).map_err(|err| {
        ProfileReadError::Engine(EngineError::Other(format!(
            "invalid legacy uploads owner {}: {err}",
            path.display()
        )))
    })
}

fn publish_legacy_uploads_owner(
    path: &Path,
    owner: &LegacyUploadsOwner,
) -> Result<(), EngineError> {
    let mut bytes = serde_json::to_vec_pretty(owner)
        .map_err(|err| EngineError::Other(format!("serialize legacy uploads owner: {err}")))?;
    bytes.push(b'\n');
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(LEGACY_UPLOADS_OWNER_FILE);
    let temp_path = path.with_file_name(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let write_result = (|| -> Result<(), EngineError> {
        let mut temp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        temp.write_all(&bytes)?;
        temp.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }

    let publish_result = std::fs::hard_link(&temp_path, path);
    let _ = std::fs::remove_file(&temp_path);
    match publish_result {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Filesystem-safe form of an org/user id used by the historical synced layout.
fn sanitize_path_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_destination_matches_the_existing_verified_storage_tuple() {
        use sha2::{Digest, Sha256};
        let data = Path::new("/isolated/crew-data");
        let binding = serde_json::to_vec(&(
            "scaffold-sandbox-storage-v1", "https://control", "https://edge", "owner", "project",
            Some("deployment"), "deployment", "session", "sandbox",
        )).unwrap();
        let expected = data.join("profiles/scaffold").join(format!("{:x}", Sha256::digest(binding)));
        let actual = scaffold_profile_directory(data, "https://control", "https://edge", "owner", "project",
            Some("deployment"), "deployment", "session", "sandbox");
        assert_eq!(actual, expected);
        for (owner, session, sandbox) in [("other", "session", "sandbox"), ("owner", "other", "sandbox"), ("owner", "session", "other")] {
            assert_ne!(actual, scaffold_profile_directory(data, "https://control", "https://edge", owner, "project",
                Some("deployment"), "deployment", session, sandbox));
        }
    }


    #[test]
    fn local_profile_id_is_stable_and_not_the_dev_identity() {
        let dir = tempfile::tempdir().unwrap();
        let first = EngineProfile::local(dir.path()).unwrap();
        let second = EngineProfile::local(dir.path()).unwrap();

        assert!(!first.user_id().is_empty());
        assert_ne!(first.user_id(), "dev-user");
        assert_eq!(first.user_id(), second.user_id());
        assert!(Uuid::parse_str(first.user_id()).is_ok());
    }

    #[test]
    fn local_profiles_in_different_data_dirs_have_different_ids() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();

        let first = EngineProfile::local(first_dir.path()).unwrap();
        let second = EngineProfile::local(second_dir.path()).unwrap();

        assert_ne!(first.user_id(), second.user_id());
    }

    #[test]
    fn local_profile_resolves_isolated_store_and_upload_roots() {
        let dir = tempfile::tempdir().unwrap();
        let profile = EngineProfile::local(dir.path()).unwrap();
        let local_root = dir.path().join("profiles/local");

        assert_eq!(profile.scope(), WorkspaceScope::Local);
        assert_eq!(profile.store_root(), local_root);
        assert_eq!(profile.uploads_root(), local_root.join("uploads"));
    }

    #[test]
    fn synced_profile_uses_account_scoped_roots() {
        let dir = tempfile::tempdir().unwrap();
        let profile = EngineProfile::synced(dir.path(), "org/example", "user@example.com");

        assert_eq!(profile.scope(), WorkspaceScope::Synced);
        assert_eq!(
            profile.store_root(),
            dir.path().join("orgs/org_example/user_example_com")
        );
        assert_eq!(
            profile.uploads_root(),
            dir.path().join("orgs/org_example/user_example_com/uploads")
        );
    }

    #[test]
    fn legacy_uploads_are_claimed_by_only_one_account() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("uploads")).unwrap();
        let first = EngineProfile::synced(dir.path(), "org-a", "user-a");
        let second = EngineProfile::synced(dir.path(), "org-b", "user-b");

        assert_eq!(
            first.claim_legacy_uploads_root().unwrap(),
            Some(dir.path().join("uploads"))
        );
        assert_eq!(second.claim_legacy_uploads_root().unwrap(), None);
        assert_eq!(
            first.claim_legacy_uploads_root().unwrap(),
            Some(dir.path().join("uploads"))
        );
    }

    #[tokio::test]
    async fn profile_assembler_uses_the_resolved_local_roots() {
        let dir = tempfile::tempdir().unwrap();
        let profile = EngineProfile::local(dir.path()).unwrap();
        let core = crate::EngineCore::assemble_with_profile(
            profile,
            std::sync::Arc::new(crate::default_registry()),
            zeron_proto::HarnessId::Mock,
            None,
        )
        .unwrap();

        assert_eq!(core.workspace_scope(), WorkspaceScope::Local);
        assert_eq!(
            core.uploads.dir(),
            dir.path().join("profiles/local/uploads")
        );
        assert!(dir.path().join("profiles/local").is_dir());
        assert!(!dir.path().join("orgs/dev-org/dev-user").exists());
        core.shutdown().await;
    }

    #[tokio::test]
    async fn development_constructor_uses_account_scoped_uploads() {
        let dir = tempfile::tempdir().unwrap();
        let org_id = crate::env_or("ZERON_ORG_ID", crate::DEFAULT_ORG_ID);
        let user_id = crate::env_or("ZERON_USER_ID", crate::DEFAULT_USER_ID);
        let expected = EngineProfile::development(dir.path(), &org_id, &user_id);
        let core = crate::EngineCore::assemble(
            dir.path(),
            std::sync::Arc::new(crate::default_registry()),
            zeron_proto::HarnessId::Mock,
            None,
        )
        .unwrap();

        assert_eq!(core.workspace_scope(), WorkspaceScope::Development);
        assert_eq!(core.uploads.dir(), expected.store_root().join("uploads"));
        assert!(expected.store_root().is_dir());
        if std::env::var("ZERON_ORG_ID").is_err() && std::env::var("ZERON_USER_ID").is_err() {
            assert_eq!(
                expected.store_root(),
                dir.path().join("orgs/dev-org/dev-user")
            );
        }
        core.shutdown().await;
    }
}
