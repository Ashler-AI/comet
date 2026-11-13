//! Crew: the Scaffold profile (docs/crew/zgx-thin-port.md, slice 2), ported
//! from Crew's `crates/engine/src/profile.rs` (Ashler-AI/comet). A child of
//! `profile`, so upstream's `EngineProfile` gains one field and no logic.
//!
//! A Scaffold profile opens only for a verified identity: its store is
//! `profiles/scaffold/<profile key>`, the key a hash of the authority's
//! origins, the project scope, the deployment and the user (and a device
//! host's grant), so paths never follow the live auth service elsewhere.
//!
//! Verified sandbox storage is session-scoped, not device/epoch-scoped. A durable
//! binding under `profiles/scaffold` retains the current authority's legacy root
//! in place (including absolute upload paths); authentication keeps its own key.
//! Both the binding and its directory must travel with a checkpoint. Older-epoch
//! legacy roots without that prior verified binding require explicit recovery;
//! they cannot be identified from a new credential and are never scanned/imported.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeron_proto::WorkspaceScope;

use super::EngineProfile;
use crate::EngineError;

const MAX_SCAFFOLD_STORE_BINDING_BYTES: u64 = 4096;

/// What a Scaffold profile carries beyond upstream's (`EngineProfile::crew`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CrewProfile {
    managed_scaffold: bool,
    device_id_override: Option<String>,
    scaffold_profile_key: Option<String>,
}

impl EngineProfile {
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
            crew: CrewProfile {
                managed_scaffold: true,
                device_id_override: session.device.map(|device| device.device_id),
                scaffold_profile_key: Some(key),
            },
        })
    }

    /// A Scaffold profile: nothing is imported into it implicitly.
    pub fn managed_scaffold(&self) -> bool {
        self.crew.managed_scaffold
    }

    /// A sandbox host's device id (its grant's), instead of the data dir's.
    pub fn device_id_override(&self) -> Option<&str> {
        self.crew.device_id_override.as_deref()
    }

    /// The verified identity this profile was opened for.
    pub fn scaffold_profile_key(&self) -> Option<&str> {
        self.crew.scaffold_profile_key.as_deref()
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
