#![cfg(target_os = "macos")]

use std::io::{BufRead as _, Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use comet_update::{
    FileMeta, Manifest, apply_mac_app, fetch_latest, mac_app_artifact, stage_mac_app,
};
use sha2::{Digest as _, Sha256};

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

fn serve_release(bundle: &Path, archive: &Path, manifest_name: &str) -> String {
    assert!(
        std::process::Command::new("/usr/bin/tar")
            .arg("-czf")
            .arg(archive)
            .arg("-C")
            .arg(bundle.parent().unwrap())
            .arg(bundle.file_name().unwrap())
            .status()
            .unwrap()
            .success()
    );
    let mut file = std::fs::File::open(archive).unwrap();
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let artifact = mac_app_artifact("1.2.3");
    let manifest = Manifest {
        version: "1.2.3".into(),
        files: [(
            artifact.clone(),
            FileMeta {
                sha256: format!("{:x}", hash.finalize()),
            },
        )]
        .into_iter()
        .collect(),
    };
    let manifest = serde_json::to_vec(&manifest).unwrap();
    let manifest_path = format!("GET /api/releases/{manifest_name} HTTP/1.1\r\n");
    let artifact_path = format!("GET /api/releases/{artifact} HTTP/1.1\r\n");
    let archive = archive.to_path_buf();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for expected in [manifest_path, artifact_path] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .unwrap();
            let mut reader = std::io::BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, expected);
            let mut authorized = false;
            loop {
                line.clear();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                authorized |= line
                    .trim()
                    .eq_ignore_ascii_case("authorization: Bearer fixture-token");
            }
            assert!(authorized);
            drop(reader);
            if expected.contains("manifest.json") {
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", manifest.len()).unwrap();
                stream.write_all(&manifest).unwrap();
            } else {
                let mut file = std::fs::File::open(&archive).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    file.metadata().unwrap().len()
                )
                .unwrap();
                std::io::copy(&mut file, &mut stream).unwrap();
            }
        }
    });
    origin
}

// Run in both COMET_PACKAGE_ENVIRONMENT=production and =staging builds with the
// real compile-time signing-team pin and extracted, signed/notarized fixtures.
#[tokio::test]
#[ignore = "requires signed production and staging distribution fixtures"]
async fn signed_updates_preserve_package_identity() {
    let production = PathBuf::from(std::env::var_os("COMET_UPDATE_PRODUCTION_FIXTURE").unwrap());
    let staging = PathBuf::from(std::env::var_os("COMET_UPDATE_STAGING_FIXTURE").unwrap());
    let (incoming, opposite, app_name, manifest_name) =
        if option_env!("COMET_PACKAGE_ENVIRONMENT") == Some("staging") {
            (
                &staging,
                &production,
                "Crew Staging.app",
                "desktop-staging-manifest.json",
            )
        } else {
            (&production, &staging, "Crew.app", "desktop-manifest.json")
        };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let origin = serve_release(incoming, &root.join("release.tar.gz"), manifest_name);
    let manifest = fetch_latest(&origin, Some("fixture-token")).await.unwrap();
    let staged = stage_mac_app(
        &origin,
        Some("fixture-token"),
        &manifest,
        &root.join("data"),
    )
    .await
    .unwrap();
    assert_eq!(staged.file_name().unwrap(), app_name);
    assert_eq!(
        stage_mac_app("http://127.0.0.1:1", None, &manifest, &root.join("data"))
            .await
            .unwrap(),
        staged
    );
    let renamed = staged.with_file_name("Renamed.app");
    std::fs::rename(&staged, &renamed).unwrap();
    let staged = renamed;

    let installed = root.join("Applications").join(app_name);
    copy_bundle(incoming, &installed);
    let original = snapshot(&installed);
    assert!(apply_mac_app(opposite, &installed).is_err());
    assert_eq!(snapshot(&installed), original);

    std::fs::remove_dir_all(&installed).unwrap();
    copy_bundle(opposite, &installed);
    let original = snapshot(&installed);
    assert!(apply_mac_app(&staged, &installed).is_err());
    assert_eq!(snapshot(&installed), original);

    let legacy = root.join("Applications/Comet.app");
    copy_bundle(incoming, &legacy);
    let old_legacy = snapshot(&legacy);
    assert!(apply_mac_app(&staged, &legacy).is_err());
    assert_eq!(snapshot(&installed), original);
    assert_eq!(snapshot(&legacy), old_legacy);

    std::fs::remove_dir_all(&installed).unwrap();
    let updated = apply_mac_app(&staged, &legacy).unwrap();
    assert_eq!(updated, installed);
    assert!(!legacy.exists());
    assert_eq!(snapshot(&updated).1, snapshot(incoming).1);
    assert_eq!(apply_mac_app(&staged, &updated).unwrap(), installed);

    // Give the opposite identity the expected directory name: neither the
    // download nor the cached fast path may trust a bundle's filename.
    let wrong = root.join("wrong").join(app_name);
    copy_bundle(opposite, &wrong);
    let origin = serve_release(&wrong, &root.join("wrong.tar.gz"), manifest_name);
    let manifest = fetch_latest(&origin, Some("fixture-token")).await.unwrap();
    let data = root.join("wrong-data");
    let original = snapshot(&installed);
    assert!(
        stage_mac_app(&origin, Some("fixture-token"), &manifest, &data)
            .await
            .is_err()
    );
    let cached = data.join("updates/1.2.3").join(app_name);
    let cached_original = snapshot(&cached);
    assert!(
        stage_mac_app("http://127.0.0.1:1", None, &manifest, &data)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&cached), cached_original);
    assert_eq!(snapshot(&installed), original);
}
