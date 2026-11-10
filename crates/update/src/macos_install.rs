//! macOS app updates, verified ahead of the swap.
//!
//! The release is unpacked straight into a hidden directory beside the
//! installed app (same volume, so the swap is an atomic rename). That copy is
//! the only bundle that gets the full `codesign --deep` + Gatekeeper
//! verification, and it gets it in the background, before "restart to apply"
//! is offered. Those checks took minutes on a busy Mac; run at restart, on the
//! UI thread, they froze the app until it was killed mid-update.
//!
//! The swap must not trust a verification that happened earlier unless
//! nothing has changed since. Every verified bundle is sealed: each entry's
//! path, inode, mode, owner, size, and modification time, plus every file's
//! change time, recorded before and after verification. Any write, chmod, or
//! xattr change to a file moves its change time, which no unprivileged process
//! can set back; adding, removing, renaming, or replacing any entry changes the
//! walk or an inode. At swap time, an identical seal proves the bundle holds
//! exactly the bytes that were verified.
//!
//! Directories are sealed without their change time. macOS writes an xattr to
//! a bundle's root directory some seconds after the bundle first runs or is
//! assessed (seen 10-14 s after `--version` and after launch, on a Mac running
//! an endpoint agent), which moves only that directory's change time. Sealing
//! it would send every restart down the slow path. A directory's change time
//! adds nothing else: its entries and their contents are sealed on their own,
//! and an xattr or ACL on a directory changes no code that runs.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

use crate::macos_verification::Policy;
use crate::{Manifest, mac_app_name};

/// The hidden directory beside `bundle` that holds the prepared update.
fn update_dir(bundle: &Path) -> anyhow::Result<PathBuf> {
    let parent = bundle
        .parent()
        .context("app bundle has no parent directory")?;
    Ok(parent.join(format!(".{}.update", mac_app_name())))
}

fn prepared_bundle(bundle: &Path) -> anyhow::Result<PathBuf> {
    Ok(update_dir(bundle)?.join(mac_app_name()))
}

/// Metadata of every entry in a bundle tree, in walk order.
#[derive(Debug, PartialEq, Eq)]
struct Seal(Vec<(PathBuf, Entry)>);

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    size: u64,
    mtime: (i64, i64),
    /// `None` for directories (see the module docs).
    ctime: Option<(i64, i64)>,
    link: Option<PathBuf>,
}

fn seal(root: &Path) -> anyhow::Result<Seal> {
    let mut entries = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let link = metadata
            .file_type()
            .is_symlink()
            .then(|| std::fs::read_link(&path))
            .transpose()?;
        if metadata.is_dir() {
            let mut children = std::fs::read_dir(&path)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<Result<Vec<_>, _>>()?;
            children.sort();
            pending.extend(children.into_iter().rev());
        }
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        entries.push((
            relative,
            Entry {
                dev: metadata.dev(),
                ino: metadata.ino(),
                mode: metadata.mode(),
                uid: metadata.uid(),
                size: metadata.size(),
                mtime: (metadata.mtime(), metadata.mtime_nsec()),
                ctime: (!metadata.is_dir()).then(|| (metadata.ctime(), metadata.ctime_nsec())),
                link,
            },
        ));
    }
    Ok(Seal(entries))
}

#[derive(Debug, Clone, Copy)]
enum Role {
    /// The incoming release: Developer ID, pinned team, and Gatekeeper.
    Distribution,
    /// An installed bundle the swap would move aside.
    Installed,
}

impl Role {
    fn verify(self, policy: &Policy, bundle: &Path) -> anyhow::Result<()> {
        match self {
            Self::Distribution => policy.verify_distribution(bundle),
            Self::Installed => policy.verify_installed(bundle),
        }
    }
}

/// What [`Prepared::commit`] does with a bundle that changed since staging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Changed {
    /// Verify it again in full (minutes on a busy Mac), then swap.
    Reverify,
    /// Refuse the swap and leave everything in place, so the caller is never
    /// held up; the next staging verifies the prepared update again.
    Refuse,
}

#[derive(Debug)]
struct Sealed {
    path: PathBuf,
    seal: Seal,
}

/// Verify `path`, proving no entry changed while the checks ran.
fn verify_sealed(policy: &Policy, path: &Path, role: Role) -> anyhow::Result<Sealed> {
    let before = seal(path)?;
    role.verify(policy, path)?;
    let seal = seal(path)?;
    anyhow::ensure!(
        before == seal,
        "{} changed while it was being verified",
        path.display()
    );
    Ok(Sealed {
        path: path.to_path_buf(),
        seal,
    })
}

/// A verified update beside the installed app, ready for a fast swap.
#[derive(Debug)]
pub(crate) struct Prepared {
    fresh: PathBuf,
    bundle: PathBuf,
    sealed: Vec<Sealed>,
}

impl Prepared {
    pub(crate) fn path(&self) -> &Path {
        &self.fresh
    }

    pub(crate) fn bundle(&self) -> &Path {
        &self.bundle
    }

    /// Verify the prepared copy and every installed bundle the swap would
    /// move. Blocking: this is the slow part.
    fn verify(bundle: &Path) -> anyhow::Result<Self> {
        let policy = Policy::pinned()?;
        let fresh = prepared_bundle(bundle)?;
        let mut sealed = vec![
            verify_sealed(&policy, &fresh, Role::Distribution)?,
            verify_sealed(&policy, bundle, Role::Installed)?,
        ];
        let target = target(bundle)?;
        if target != bundle && target.exists() {
            sealed.push(verify_sealed(&policy, &target, Role::Installed)?);
        }
        Ok(Self {
            fresh,
            bundle: bundle.to_path_buf(),
            sealed,
        })
    }

    /// [`Self::verify`] on a blocking thread, then run the verified binary's
    /// `--version`: it proves the payload is complete and is the release the
    /// manifest promised. Never executed before it is verified.
    async fn verify_release(bundle: &Path, version: &str) -> anyhow::Result<Self> {
        let owned = bundle.to_path_buf();
        let prepared = tokio::task::spawn_blocking(move || Self::verify(&owned))
            .await
            .context("the update verification thread stopped")??;
        crate::verify_staged_binary(&prepared.fresh.join("Contents/MacOS/comet"), version).await?;
        forget_in_launch_services(&prepared.fresh).await;
        prepared.unchanged(&prepared.fresh)?;
        Ok(prepared)
    }

    fn unchanged(&self, path: &Path) -> anyhow::Result<()> {
        let current = seal(path)?;
        anyhow::ensure!(
            self.sealed
                .iter()
                .any(|sealed| sealed.path == path && sealed.seal == current),
            "{} changed after it was verified",
            path.display()
        );
        Ok(())
    }

    /// Prove `path` is still what was verified, or else verify it again in
    /// full before it can be moved, or refuse.
    fn recheck(
        &self,
        policy: &mut Option<Policy>,
        path: &Path,
        role: Role,
        changed: Changed,
    ) -> anyhow::Result<()> {
        let Err(difference) = self.unchanged(path) else {
            return Ok(());
        };
        if changed == Changed::Refuse {
            return Err(difference);
        }
        tracing::warn!(error = %difference, "verifying the bundle again before the update swap");
        let policy = match policy {
            Some(policy) => policy,
            None => policy.insert(Policy::pinned()?),
        };
        role.verify(policy, path)
    }

    /// Swap the verified copy over the installed app, migrating renamed
    /// installs to the canonical name. Sub-second when nothing changed since
    /// [`Self::verify`]. Every trust check precedes the first rename or
    /// removal of installed data, and the original install is restored if a
    /// rename fails.
    pub(crate) fn commit(&self, changed: Changed) -> anyhow::Result<PathBuf> {
        let bundle = self.bundle.as_path();
        let parent = bundle
            .parent()
            .context("app bundle has no parent directory")?;
        let target = target(bundle)?;
        let current_name = bundle
            .file_name()
            .context("app bundle has no name")?
            .to_string_lossy();
        let target_name = mac_app_name();
        let pid = std::process::id();
        let old = parent.join(format!(".{current_name}.old-{pid}"));
        let displaced_target = (target.as_path() != bundle && target.exists())
            .then(|| parent.join(format!(".{target_name}.old-{pid}")));
        // Do not discard an interrupted update's backups or follow a stale symlink.
        for path in std::iter::once(&old).chain(displaced_target.iter()) {
            if path.symlink_metadata().is_ok() {
                bail!("update work path already exists: {}", path.display());
            }
        }
        let mut policy = None;
        let refused = "Crew update refused before replacing the installed app";
        self.recheck(&mut policy, &self.fresh, Role::Distribution, changed)
            .context(refused)?;
        self.recheck(&mut policy, bundle, Role::Installed, changed)
            .context(refused)?;
        if displaced_target.is_some() {
            self.recheck(&mut policy, &target, Role::Installed, changed)
                .context(refused)?;
        }
        if let Some(displaced) = displaced_target.as_ref() {
            std::fs::rename(&target, displaced).context("moving the existing target app aside")?;
        }
        if let Err(err) = std::fs::rename(bundle, &old) {
            if let Some(displaced) = displaced_target.as_ref() {
                let _ = std::fs::rename(displaced, &target);
            }
            return Err(err).context("moving the current app aside");
        }
        if let Err(err) = std::fs::rename(&self.fresh, &target) {
            let _ = std::fs::rename(&old, bundle);
            if let Some(displaced) = displaced_target.as_ref() {
                let _ = std::fs::rename(displaced, &target);
            }
            return Err(err).context("installing the new app bundle");
        }
        let _ = std::fs::remove_dir_all(&old);
        if let Some(displaced) = displaced_target {
            let _ = std::fs::remove_dir_all(displaced);
        }
        if let Ok(dir) = update_dir(bundle) {
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(target)
    }
}

fn target(bundle: &Path) -> anyhow::Result<PathBuf> {
    Ok(bundle
        .parent()
        .context("app bundle has no parent directory")?
        .join(mac_app_name()))
}

/// Running the staged binary's `--version` registers the staged bundle with
/// LaunchServices (seen on macOS 26), which could then launch it by bundle id
/// (a notification click, `open -b`) in place of the installed app. Unregister
/// it; this leaves the bundle's metadata alone. Best effort.
async fn forget_in_launch_services(bundle: &Path) {
    const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    let unregister = tokio::process::Command::new(LSREGISTER)
        .arg("-u")
        .arg(bundle)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(std::time::Duration::from_secs(60), unregister).await {
        Ok(Ok(output)) if output.status.success() => {}
        outcome => tracing::debug!(
            ?outcome,
            "could not unregister the staged update from LaunchServices"
        ),
    }
}

/// The version a prepared bundle claims, read without executing anything.
fn claimed_version(bundle: &Path) -> Option<String> {
    crate::bundle_short_version(&std::fs::read_to_string(bundle.join("Contents/Info.plist")).ok()?)
}

/// Download the release, unpack it beside `bundle`, and verify it there.
/// A copy prepared by an earlier run is reused only after a full verification.
pub(crate) async fn stage(
    edge_url: &str,
    access_token: Option<&str>,
    manifest: &Manifest,
    data_dir: &Path,
    bundle: &Path,
) -> anyhow::Result<Prepared> {
    let version = &manifest.version;
    let update_dir = update_dir(bundle)?;
    // Before this layout, updates were staged (and copied from) here.
    let downloads = data_dir.join("updates");
    let prepared = prepared_bundle(bundle)?;
    // Only a copy of this very release is worth minutes of verification.
    if prepared.join("Contents/MacOS/comet").exists()
        && claimed_version(&prepared).as_deref() == Some(version.as_str())
    {
        match Prepared::verify_release(bundle, version).await {
            Ok(prepared) => {
                let _ = std::fs::remove_dir_all(&downloads);
                return Ok(prepared);
            }
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "discarding the prepared update")
            }
        }
    }
    let _ = std::fs::remove_dir_all(&update_dir);
    let _ = std::fs::remove_dir_all(&downloads);
    let dir = downloads.join(version);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = crate::mac_app_artifact(version);
    let tarball = dir.join(&file);
    let result = async {
        crate::download_release_file(edge_url, access_token, manifest, &file, &tarball).await?;
        std::fs::create_dir_all(&update_dir)
            .with_context(|| format!("creating {}", update_dir.display()))?;
        let (archive, destination) = (tarball.clone(), update_dir.clone());
        tokio::task::spawn_blocking(move || {
            crate::run(
                "tar",
                &[
                    "-xzf",
                    &archive.to_string_lossy(),
                    "-C",
                    &destination.to_string_lossy(),
                ],
            )
        })
        .await
        .context("the update unpack thread stopped")??;
        if !prepared.join("Contents/MacOS/comet").exists() {
            bail!("app tarball {file} did not contain {}", mac_app_name());
        }
        Prepared::verify_release(bundle, version).await
    }
    .await;
    let _ = std::fs::remove_dir_all(&downloads);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&update_dir);
    }
    result
}

/// Copy an already unpacked release beside `bundle` and verify it there.
pub(crate) fn prepare_copy(staged: &Path, bundle: &Path) -> anyhow::Result<Prepared> {
    let update_dir = update_dir(bundle)?;
    // Clearing the work directory must never delete the source.
    anyhow::ensure!(
        !staged.starts_with(&update_dir),
        "{} is already prepared; install it with install_mac_app",
        staged.display()
    );
    let _ = std::fs::remove_dir_all(&update_dir);
    std::fs::create_dir_all(&update_dir)
        .with_context(|| format!("creating {}", update_dir.display()))?;
    let result = crate::run(
        "/usr/bin/ditto",
        &[
            &staged.to_string_lossy(),
            &prepared_bundle(bundle)?.to_string_lossy(),
        ],
    )
    .context("copying the Crew update")
    .and_then(|()| Prepared::verify(bundle));
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&update_dir);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let macos = tmp.path().join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(macos.join("comet"), b"main").unwrap();
        std::fs::write(tmp.path().join("Contents/Info.plist"), b"plist").unwrap();
        tmp
    }

    fn set_xattr(path: &Path) {
        assert!(
            std::process::Command::new("/usr/bin/xattr")
                .args(["-w", "com.example.tamper", "1"])
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn seal_is_stable_without_changes() {
        let tmp = tree();
        assert_eq!(seal(tmp.path()).unwrap(), seal(tmp.path()).unwrap());
    }

    #[test]
    fn seal_detects_every_kind_of_tampering() {
        let changes: [fn(&Path); 7] = [
            // Same-size rewrite with the original modification time restored.
            |root| {
                let file = root.join("Contents/MacOS/comet");
                let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
                std::fs::write(&file, b"evil").unwrap();
                std::fs::File::options()
                    .write(true)
                    .open(&file)
                    .unwrap()
                    .set_modified(mtime)
                    .unwrap();
            },
            // Replaced by a new file.
            |root| {
                let file = root.join("Contents/MacOS/comet");
                std::fs::remove_file(&file).unwrap();
                std::fs::write(&file, b"main").unwrap();
            },
            // An added file.
            |root| std::fs::write(root.join("Contents/MacOS/extra"), b"").unwrap(),
            // An added file with the directory's modification time restored.
            |root| {
                let dir = root.join("Contents/MacOS");
                let mtime = std::fs::metadata(&dir).unwrap().modified().unwrap();
                std::fs::write(dir.join("extra"), b"").unwrap();
                std::fs::File::open(&dir)
                    .unwrap()
                    .set_modified(mtime)
                    .unwrap();
            },
            // A permission change.
            |root| {
                use std::os::unix::fs::PermissionsExt as _;
                let file = root.join("Contents/Info.plist");
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o777)).unwrap();
            },
            // An extended attribute on a file.
            |root| set_xattr(&root.join("Contents/Info.plist")),
            // A file swapped for a symlink.
            |root| {
                let file = root.join("Contents/Info.plist");
                std::fs::remove_file(&file).unwrap();
                std::os::unix::fs::symlink("/etc/hosts", &file).unwrap();
            },
        ];
        for change in changes {
            let tmp = tree();
            let before = seal(tmp.path()).unwrap();
            // Change times have nanosecond resolution on APFS; make sure the
            // tamper cannot land in the same tick as the seal.
            std::thread::sleep(std::time::Duration::from_millis(5));
            change(tmp.path());
            assert_ne!(seal(tmp.path()).unwrap(), before);
        }
    }

    // macOS tags a bundle's root directory with an xattr seconds after it
    // first runs. That must not invalidate the verification.
    #[test]
    fn seal_survives_an_xattr_on_a_directory() {
        let tmp = tree();
        let before = seal(tmp.path()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        set_xattr(tmp.path());
        set_xattr(&tmp.path().join("Contents"));
        assert_eq!(seal(tmp.path()).unwrap(), before);
    }

    #[test]
    fn claimed_version_reads_the_plist_without_running_anything() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("Contents")).unwrap();
        assert_eq!(claimed_version(tmp.path()), None);
        std::fs::write(
            tmp.path().join("Contents/Info.plist"),
            "<plist><dict><key>CFBundleShortVersionString</key>\n\t<string>0.2.146</string></dict></plist>",
        )
        .unwrap();
        assert_eq!(claimed_version(tmp.path()).as_deref(), Some("0.2.146"));
    }

    #[test]
    fn preparing_a_copy_never_deletes_its_source() {
        let tmp = tempfile::tempdir().unwrap();
        let installed = tmp.path().join(mac_app_name());
        let source = prepared_bundle(&installed).unwrap();
        std::fs::create_dir_all(source.join("Contents")).unwrap();
        assert!(prepare_copy(&source, &installed).is_err());
        assert!(source.join("Contents").exists());
    }

    // Needs a real signed distribution: run with the release build's
    // COMET_MACOS_SIGNING_TEAM_ID and COMET_UPDATE_STAGING_FIXTURE (or the
    // production fixture for a production build) pointing at a notarized app.
    #[test]
    #[ignore = "requires a signed and notarized Crew distribution"]
    fn signed_prepared_update_swaps_fast_and_refuses_tampering() {
        let variable = if crate::staging_mac_app() {
            "COMET_UPDATE_STAGING_FIXTURE"
        } else {
            "COMET_UPDATE_PRODUCTION_FIXTURE"
        };
        let fixture = PathBuf::from(std::env::var_os(variable).unwrap());
        let temp = tempfile::tempdir().unwrap();
        let installed = temp.path().join(mac_app_name());
        crate::run(
            "/usr/bin/ditto",
            &[&fixture.to_string_lossy(), &installed.to_string_lossy()],
        )
        .unwrap();

        // Tampering after verification is caught at the swap, and the
        // installed app is left alone.
        let prepared = prepare_copy(&fixture, &installed).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(prepared.path().join("Contents/Resources/tampered"), b"x").unwrap();
        let installed_seal = seal(&installed).unwrap();
        assert!(prepared.commit(Changed::Refuse).is_err());
        assert!(prepared.commit(Changed::Reverify).is_err());
        assert_eq!(seal(&installed).unwrap(), installed_seal);

        // Untouched, the swap needs no second codesign/Gatekeeper pass, even
        // after the staged binary ran and macOS tagged the bundle for it.
        let started = std::time::Instant::now();
        let prepared = prepare_copy(&fixture, &installed).unwrap();
        eprintln!("prepare (full verification): {:?}", started.elapsed());
        let version = std::process::Command::new(prepared.path().join("Contents/MacOS/comet"))
            .arg("--version")
            .output()
            .unwrap();
        assert!(version.status.success());
        std::thread::sleep(std::time::Duration::from_secs(30));
        let started = std::time::Instant::now();
        assert_eq!(prepared.commit(Changed::Refuse).unwrap(), installed);
        let swap = started.elapsed();
        eprintln!("swap: {swap:?}");
        assert!(
            swap < std::time::Duration::from_secs(2),
            "swap took {swap:?}"
        );
        assert!(!update_dir(&installed).unwrap().exists());
        assert!(
            std::process::Command::new("/usr/bin/codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(&installed)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn prepared_update_lives_beside_the_install() {
        let bundle = Path::new("/Applications/Renamed.app");
        assert_eq!(
            prepared_bundle(bundle).unwrap(),
            Path::new("/Applications")
                .join(format!(".{}.update", mac_app_name()))
                .join(mac_app_name())
        );
    }
}
