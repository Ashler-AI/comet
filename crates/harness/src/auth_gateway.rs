use std::path::{Path, PathBuf};

use crate::HarnessError;

pub(crate) const EXTENSION_SOURCE: &str = include_str!("prime_agent_auth_gateway.ts");
const DISCOVERED_EXTENSION_SOURCE: &str = include_str!("crew_auth_gateway.ts");
const DISCOVERED_OWNERSHIP_MARKER: &[u8] = b"// @crew-managed auth-gateway v1";

struct ExtensionSnapshot {
    bytes: Vec<u8>,
    #[cfg(unix)]
    identity: DirectoryIdentity,
}

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

fn lock_installer(directory: &Path) -> Result<std::fs::File, HarnessError> {
    let path = directory.join(".auth-gateway.lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let lock = options.open(&path)?;
    let metadata = lock.metadata()?;
    if !metadata.is_file() {
        return Err(HarnessError::Protocol(format!(
            "gateway installer lock is not a regular file: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::{fd::AsRawFd as _, unix::fs::MetadataExt as _};
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
            return Err(HarnessError::Protocol(format!(
                "gateway installer lock is not exclusively owned by the current user: {}",
                path.display()
            )));
        }
        loop {
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
        let current = std::fs::symlink_metadata(&path)?;
        if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
            return Err(HarnessError::Protocol(format!(
                "gateway installer lock changed while acquiring it: {}",
                path.display()
            )));
        }
    }
    // Closing this file releases the advisory lock after installation completes.
    Ok(lock)
}

fn read_extension(path: &Path) -> Result<Option<ExtensionSnapshot>, HarnessError> {
    use std::io::Read as _;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(HarnessError::Protocol(format!(
            "gateway extension is not a regular file: {}",
            path.display()
        )));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(HarnessError::Protocol(format!(
                "gateway extension is not owned by the current user: {}",
                path.display()
            )));
        }
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let mut file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Err(HarnessError::Protocol(format!(
            "gateway extension changed before opening: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt as _;
        if opened.dev() != metadata.dev()
            || opened.ino() != metadata.ino()
            || opened.uid() != metadata.uid()
        {
            return Err(HarnessError::Protocol(format!(
                "gateway extension changed before opening: {}",
                path.display()
            )));
        }
        DirectoryIdentity {
            device: opened.dev(),
            inode: opened.ino(),
            uid: opened.uid(),
        }
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(ExtensionSnapshot {
        bytes,
        #[cfg(unix)]
        identity,
    }))
}

fn publish_extension(
    directory: &Path,
    path: &Path,
    source: &str,
    previous: Option<&ExtensionSnapshot>,
) -> Result<(), HarnessError> {
    use std::io::Write as _;

    if previous.is_some_and(|snapshot| snapshot.bytes == source.as_bytes()) {
        return Ok(());
    }
    // Stage outside the discovery directory so OMP can never load partial bytes.
    let temporary = directory.join(format!(".gateway-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(source.as_bytes())?;
        file.sync_all()?;
        if let Some(previous) = previous {
            let current = read_extension(path)?.ok_or_else(|| {
                HarnessError::Protocol(format!(
                    "gateway extension disappeared before publication: {}",
                    path.display()
                ))
            })?;
            if current.bytes == source.as_bytes() {
                return Ok(());
            }
            let unchanged = current.bytes == previous.bytes;
            #[cfg(unix)]
            let unchanged = unchanged && current.identity == previous.identity;
            if !unchanged {
                return Err(HarnessError::Protocol(format!(
                    "gateway extension changed before publication: {}",
                    path.display()
                )));
            }
            std::fs::rename(&temporary, path)?;
        } else {
            // Unlike rename, linking refuses to replace a concurrently created override.
            if let Err(error) = std::fs::hard_link(&temporary, path) {
                if error.kind() != std::io::ErrorKind::AlreadyExists
                    || !read_extension(path)?
                        .is_some_and(|current| current.bytes == source.as_bytes())
                {
                    return Err(error.into());
                }
            }
        }
        Ok(())
    })();
    drop(file);
    let _ = std::fs::remove_file(&temporary);
    result
}

fn write_bundled_extension(directory: &Path) -> Result<PathBuf, HarnessError> {
    let path = directory.join("agent-auth-gateway.ts");
    let previous = read_extension(&path)?;
    publish_extension(directory, &path, EXTENSION_SOURCE, previous.as_ref())?;
    Ok(path)
}

pub(crate) fn install_extension(agent_dir: &Path) -> Result<(), HarnessError> {
    let directory = prepare_runtime_dir(agent_dir)?;
    let _lock = lock_installer(&directory)?;
    let extensions = agent_dir.join("extensions");
    match std::fs::symlink_metadata(&extensions) {
        Ok(metadata) => {
            // This is a shared user directory: validate it, but never chmod it.
            #[cfg(unix)]
            validate_private_directory_metadata(&extensions, &metadata, unsafe {
                libc::geteuid()
            })?;
            #[cfg(not(unix))]
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(HarnessError::Protocol(format!(
                    "extension directory is not a regular directory: {}",
                    extensions.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            prepare_private_directory(&extensions)?;
        }
        Err(error) => return Err(error.into()),
    }
    let path = extensions.join("crew-auth-gateway.ts");
    let previous = read_extension(&path)?;
    if previous.as_ref().is_some_and(|snapshot| {
        snapshot.bytes.split(|byte| *byte == b'\n').next() != Some(DISCOVERED_OWNERSHIP_MARKER)
    }) {
        return Err(HarnessError::Protocol(format!(
            "refusing to replace a user-owned gateway extension: {}",
            path.display()
        )));
    }
    if previous
        .as_ref()
        .is_some_and(|snapshot| snapshot.bytes != DISCOVERED_EXTENSION_SOURCE.as_bytes())
    {
        return Err(HarnessError::Protocol(format!(
            "refusing to replace a modified or incompatible managed gateway wrapper: {}; wrapper migrations require an explicit versioned installation",
            path.display()
        )));
    }
    let legacy = extensions.join("omp-auth-gateway.ts");
    match std::fs::symlink_metadata(&legacy) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            return Ok(());
        }
        Ok(_) => {
            return Err(HarnessError::Protocol(format!(
                "legacy gateway extension is not a regular file: {}",
                legacy.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    write_bundled_extension(&directory)?;
    if previous.is_some() {
        return Ok(());
    }
    // Discovery files are immutable. Never rename over a user replacement,
    // even if it appears after inspection and retains the ownership marker.
    publish_extension(&directory, &path, DISCOVERED_EXTENSION_SOURCE, None)
}

pub(crate) fn install_prime_extension(agent_dir: &Path) -> Result<PathBuf, HarnessError> {
    let directory = prepare_runtime_dir(agent_dir)?;
    let _lock = lock_installer(&directory)?;
    write_bundled_extension(&directory)
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
    fn installs_immutable_discovered_adapter_and_upgrades_bundled_runtime() {
        let root = tempfile::tempdir().unwrap();
        let extensions = root.path().join("extensions");
        std::fs::create_dir(&extensions).unwrap();
        let user = extensions.join("my-tools.ts");
        std::fs::write(&user, "user extension").unwrap();
        let managed = extensions.join("crew-auth-gateway.ts");
        let runtime = root.path().join("comet-runtime/agent-auth-gateway.ts");

        install_extension(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&managed).unwrap(),
            DISCOVERED_EXTENSION_SOURCE
        );
        assert_eq!(std::fs::read_to_string(&runtime).unwrap(), EXTENSION_SOURCE);
        let installed_time = std::fs::metadata(&managed).unwrap().modified().unwrap();
        install_extension(root.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&managed).unwrap().modified().unwrap(),
            installed_time
        );

        std::fs::write(&runtime, "old bundled adapter").unwrap();
        install_extension(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(managed).unwrap(),
            DISCOVERED_EXTENSION_SOURCE
        );
        assert_eq!(std::fs::read_to_string(runtime).unwrap(), EXTENSION_SOURCE);
        assert_eq!(std::fs::read_to_string(user).unwrap(), "user extension");
    }

    #[test]
    fn refuses_modified_managed_wrapper_without_replacing_it() {
        let root = tempfile::tempdir().unwrap();
        install_extension(root.path()).unwrap();
        let managed = root.path().join("extensions/crew-auth-gateway.ts");
        let modified = "// @crew-managed auth-gateway v1\nuser-modified wrapper";
        std::fs::write(&managed, modified).unwrap();

        let error = install_extension(root.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("modified or incompatible managed gateway wrapper")
        );
        assert_eq!(std::fs::read_to_string(&managed).unwrap(), modified);
        // Initial publication also refuses an override appearing after inspection.
        assert!(
            publish_extension(
                &root.path().join("comet-runtime"),
                &managed,
                DISCOVERED_EXTENSION_SOURCE,
                None,
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(managed).unwrap(), modified);
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_omp_and_prime_installers_serialize_runtime_upgrades() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("comet-runtime/agent-auth-gateway.ts");
        for upgrade in [false, true] {
            if upgrade {
                std::fs::write(&runtime, "old bundled adapter").unwrap();
            }
            let barrier = std::sync::Barrier::new(8);
            std::thread::scope(|scope| {
                for index in 0..8 {
                    let barrier = &barrier;
                    let root = root.path();
                    scope.spawn(move || {
                        barrier.wait();
                        if index % 2 == 0 {
                            install_extension(root).unwrap();
                        } else {
                            install_prime_extension(root).unwrap();
                        }
                    });
                }
            });
            assert_eq!(std::fs::read_to_string(&runtime).unwrap(), EXTENSION_SOURCE);
            assert_eq!(
                std::fs::read_to_string(root.path().join("extensions/crew-auth-gateway.ts"))
                    .unwrap(),
                DISCOVERED_EXTENSION_SOURCE,
            );
        }
    }

    #[test]
    fn concurrent_publications_accept_identical_complete_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("extension.ts");
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    barrier.wait();
                    publish_extension(root.path(), &path, DISCOVERED_EXTENSION_SOURCE, None)
                        .unwrap();
                });
            }
        });
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            DISCOVERED_EXTENSION_SOURCE
        );

        let previous = read_extension(&path).unwrap().unwrap();
        publish_extension(root.path(), &path, "upgraded bytes", Some(&previous)).unwrap();
        publish_extension(root.path(), &path, "upgraded bytes", Some(&previous)).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "upgraded bytes");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn refuses_user_owned_discovery_collision_even_with_embedded_marker() {
        let root = tempfile::tempdir().unwrap();
        let extensions = root.path().join("extensions");
        std::fs::create_dir(&extensions).unwrap();
        let managed = extensions.join("crew-auth-gateway.ts");
        let contents = "// user extension\n// @crew-managed auth-gateway v1\n";
        std::fs::write(&managed, contents).unwrap();

        assert!(install_extension(root.path()).is_err());
        assert_eq!(std::fs::read_to_string(managed).unwrap(), contents);
    }

    #[test]
    fn legacy_override_preserves_existing_managed_and_user_files() {
        let root = tempfile::tempdir().unwrap();
        install_extension(root.path()).unwrap();
        let managed = root.path().join("extensions/crew-auth-gateway.ts");
        let legacy = root.path().join("extensions/omp-auth-gateway.ts");
        let previous = std::fs::read(&managed).unwrap();
        std::fs::write(&legacy, "user gateway override").unwrap();

        install_extension(root.path()).unwrap();
        assert_eq!(std::fs::read(managed).unwrap(), previous);
        assert_eq!(
            std::fs::read_to_string(legacy).unwrap(),
            "user gateway override"
        );
    }

    #[test]
    fn refuses_nonregular_extension_collisions() {
        for name in ["crew-auth-gateway.ts", "omp-auth-gateway.ts"] {
            let root = tempfile::tempdir().unwrap();
            let collision = root.path().join("extensions").join(name);
            std::fs::create_dir_all(&collision).unwrap();
            assert!(install_extension(root.path()).is_err());
            assert!(collision.is_dir());
        }
    }

    #[test]
    fn atomic_publication_refuses_new_and_changed_collisions() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("extension.ts");
        std::fs::write(&path, "user-created collision").unwrap();
        assert!(publish_extension(root.path(), &path, "replacement", None).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "user-created collision"
        );

        let previous = read_extension(&path).unwrap().unwrap();
        std::fs::write(&path, "changed after inspection").unwrap();
        assert!(publish_extension(root.path(), &path, "replacement", Some(&previous)).is_err());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "changed after inspection"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn preserves_user_directory_and_extension_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let extensions = root.path().join("extensions");
        std::fs::create_dir(&extensions).unwrap();
        std::fs::set_permissions(&extensions, std::fs::Permissions::from_mode(0o755)).unwrap();
        let user = extensions.join("user.ts");
        std::fs::write(&user, "user extension").unwrap();
        std::fs::set_permissions(&user, std::fs::Permissions::from_mode(0o640)).unwrap();
        install_extension(root.path()).unwrap();

        let legacy = extensions.join("omp-auth-gateway.ts");
        std::fs::write(&legacy, "user gateway override").unwrap();
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o644)).unwrap();
        install_extension(root.path()).unwrap();
        assert_eq!(
            std::fs::metadata(extensions).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(user).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            std::fs::metadata(legacy).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_directories_and_adapter_paths_without_touching_targets() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        for relative in [
            "extensions",
            "extensions/crew-auth-gateway.ts",
            "extensions/omp-auth-gateway.ts",
            "comet-runtime/agent-auth-gateway.ts",
        ] {
            let root = tempfile::tempdir().unwrap();
            let target = root.path().join("target");
            if relative == "extensions" {
                std::fs::create_dir(&target).unwrap();
            } else {
                std::fs::write(&target, "user-owned target").unwrap();
            }
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
            let link = root.path().join(relative);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(&target, &link).unwrap();

            assert!(install_extension(root.path()).is_err(), "{relative}");
            assert!(
                std::fs::symlink_metadata(link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o755
            );
            if relative != "extensions" {
                assert_eq!(
                    std::fs::read_to_string(target).unwrap(),
                    "user-owned target"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_dangling_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let extensions = root.path().join("extensions");
        std::fs::create_dir(&extensions).unwrap();
        let missing = root.path().join("missing");
        symlink(&missing, extensions.join("crew-auth-gateway.ts")).unwrap();
        assert!(install_extension(root.path()).is_err());
        assert!(!missing.exists());
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

        install_extension(root.path()).unwrap();
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
