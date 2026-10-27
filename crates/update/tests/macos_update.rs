#![cfg(target_os = "macos")]

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use zeron_update::apply_mac_app;

fn copy_bundle(source: &Path, destination: &Path) {
    assert!(
        std::process::Command::new("/usr/bin/ditto")
            .arg(source)
            .arg(destination)
            .status()
            .unwrap()
            .success()
    );
}

fn snapshot(bundle: &Path) -> (u64, Vec<u8>) {
    (
        std::fs::metadata(bundle).unwrap().ino(),
        std::fs::read(bundle.join("Contents/Info.plist")).unwrap(),
    )
}

// Real distributions are required: an ad-hoc fixture must never authorize a swap.
// Run with the release build's COMET_MACOS_SIGNING_TEAM_ID and package environment.
#[test]
#[ignore = "requires signed and notarized production and staging Crew distributions"]
fn signed_updates_preserve_identity_and_canonical_install_path() {
    let production = PathBuf::from(std::env::var_os("COMET_UPDATE_PRODUCTION_FIXTURE").unwrap());
    let staging = PathBuf::from(std::env::var_os("COMET_UPDATE_STAGING_FIXTURE").unwrap());
    let (incoming, opposite, app_name) = if option_env!("COMET_PACKAGE_ENVIRONMENT")
        .or(option_env!("COMET_DEFAULT_ENVIRONMENT"))
        != Some("production")
    {
        (&staging, &production, "Crew Staging.app")
    } else {
        (&production, &staging, "Crew.app")
    };
    let temp = tempfile::tempdir().unwrap();
    let installed = temp.path().join(app_name);
    copy_bundle(incoming, &installed);
    let original = snapshot(&installed);
    assert!(apply_mac_app(opposite, &installed).is_err());
    assert_eq!(snapshot(&installed), original);

    let tampered = temp.path().join("incoming").join(app_name);
    copy_bundle(incoming, &tampered);
    std::fs::write(tampered.join("Contents/MacOS/comet"), b"tampered").unwrap();
    assert!(apply_mac_app(&tampered, &installed).is_err());
    assert_eq!(snapshot(&installed), original);

    let legacy = temp.path().join("Renamed.app");
    std::fs::rename(&installed, &legacy).unwrap();
    assert_eq!(apply_mac_app(incoming, &legacy).unwrap(), installed);
    assert!(!legacy.exists());
    assert_eq!(snapshot(&installed).1, snapshot(incoming).1);

    std::fs::remove_dir_all(&installed).unwrap();
    copy_bundle(opposite, &installed);
    let original = snapshot(&installed);
    assert!(apply_mac_app(incoming, &installed).is_err());
    assert_eq!(snapshot(&installed), original);
}
