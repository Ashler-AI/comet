use std::path::{Path, PathBuf};

use crate::HarnessError;

pub(crate) const EXTENSION_SOURCE: &str = include_str!("prime_agent_auth_gateway.ts");

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
    uid: u32,
}

#[cfg(unix)]
fn validate_private_directory_metadata(
    directory: &Path,
    metadata: &std::fs::Metadata,
    expected_uid: u32,
) -> Result<DirectoryIdentity, HarnessError> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.file_type().is_symlink() {
        return Err(HarnessError::Protocol(format!(
            "private directory path is a symbolic link: {}",
            directory.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(HarnessError::Protocol(format!(
            "private directory path is not a directory: {}",
            directory.display()
        )));
    }
    if metadata.uid() != expected_uid {
        return Err(HarnessError::Protocol(format!(
            "private directory is not owned by the current user: {}",
            directory.display()
        )));
    }
    Ok(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
    })
}

#[cfg(unix)]
fn prepare_private_directory_unix(directory: &Path) -> Result<(), HarnessError> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let expected_uid = unsafe { libc::geteuid() };
    let before = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => Some(validate_private_directory_metadata(
            directory,
            &metadata,
            expected_uid,
        )?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if before.is_none() {
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }

    let path_metadata = std::fs::symlink_metadata(directory)?;
    let path_identity =
        validate_private_directory_metadata(directory, &path_metadata, expected_uid)?;
    if before.is_some_and(|identity| identity != path_identity) {
        return Err(HarnessError::Protocol(format!(
            "private directory changed during validation: {}",
            directory.display()
        )));
    }

    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory)?;
    let opened_identity =
        validate_private_directory_metadata(directory, &opened.metadata()?, expected_uid)?;
    if opened_identity != path_identity {
        return Err(HarnessError::Protocol(format!(
            "private directory changed before secure open: {}",
            directory.display()
        )));
    }

    opened.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    let restricted_identity =
        validate_private_directory_metadata(directory, &opened.metadata()?, expected_uid)?;
    if restricted_identity != opened_identity {
        return Err(HarnessError::Protocol(format!(
            "private directory changed while restricting permissions: {}",
            directory.display()
        )));
    }
    let final_path_identity = validate_private_directory_metadata(
        directory,
        &std::fs::symlink_metadata(directory)?,
        expected_uid,
    )?;
    if final_path_identity != opened_identity {
        return Err(HarnessError::Protocol(format!(
            "private directory path changed after secure open: {}",
            directory.display()
        )));
    }
    Ok(())
}

pub(crate) fn prepare_private_directory(directory: &Path) -> Result<(), HarnessError> {
    #[cfg(unix)]
    {
        prepare_private_directory_unix(directory)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(directory)?;
        Ok(())
    }
}

pub(crate) fn prepare_runtime_dir(agent_dir: &Path) -> Result<PathBuf, HarnessError> {
    std::fs::create_dir_all(agent_dir)?;
    let directory = agent_dir.join("comet-runtime");
    prepare_private_directory(&directory)?;
    Ok(directory)
}

fn write_bundled_extension(agent_dir: &Path) -> Result<PathBuf, HarnessError> {
    let directory = prepare_runtime_dir(agent_dir)?;
    let path = directory.join("agent-auth-gateway.ts");
    let current = std::fs::read(&path).ok();
    if current.as_deref() != Some(EXTENSION_SOURCE.as_bytes()) {
        std::fs::write(&path, EXTENSION_SOURCE)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

pub(crate) fn install_extension(agent_dir: &Path) -> Result<Option<PathBuf>, HarnessError> {
    let discovered = agent_dir.join("extensions/omp-auth-gateway.ts");
    if discovered.is_file() {
        return Ok(None);
    }
    write_bundled_extension(agent_dir).map(Some)
}

pub(crate) fn install_prime_extension(agent_dir: &Path) -> Result<PathBuf, HarnessError> {
    write_bundled_extension(agent_dir)
}
pub(crate) fn provider(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("comet-openai"),
        "anthropic" => Some("comet-anthropic"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles_a_credential_free_loopback_gateway_adapter() {
        assert!(EXTENSION_SOURCE.contains("OMP_AUTH_GATEWAY_TOKEN"));
        assert!(EXTENSION_SOURCE.contains("http://127.0.0.1:4000"));
        assert!(!EXTENSION_SOURCE.contains("sk-"));
        assert!(!EXTENSION_SOURCE.contains("refreshToken"));
    }

    #[test]
    fn pins_gateway_runs_to_the_granted_provider() {
        assert_eq!(provider("openai"), Some("comet-openai"));
        assert_eq!(provider("anthropic"), Some("comet-anthropic"));
        assert_eq!(provider("prime-inference"), None);
    }

    #[test]
    fn prime_always_installs_the_namespaced_gateway_extension() {
        let root = tempfile::tempdir().unwrap();
        let discovered = root.path().join("extensions/omp-auth-gateway.ts");
        std::fs::create_dir_all(discovered.parent().unwrap()).unwrap();
        std::fs::write(&discovered, "legacy shared-provider extension").unwrap();

        assert_eq!(install_extension(root.path()).unwrap(), None);
        let installed = install_prime_extension(root.path()).unwrap();
        assert_ne!(installed, discovered);
        assert_eq!(
            std::fs::read(installed).unwrap(),
            EXTENSION_SOURCE.as_bytes()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_directory_not_owned_by_the_expected_user() {
        use std::os::unix::fs::MetadataExt as _;

        let root = tempfile::tempdir().unwrap();
        let metadata = std::fs::symlink_metadata(root.path()).unwrap();
        let foreign_uid = if metadata.uid() == 0 { 1 } else { 0 };
        let error =
            validate_private_directory_metadata(root.path(), &metadata, foreign_uid).unwrap_err();

        assert!(error.to_string().contains("not owned by the current user"));
    }
}
