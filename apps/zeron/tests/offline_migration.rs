use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path, process::Command};

fn hashes(root: &Path, names: &[&str]) -> BTreeMap<String, String> {
    names
        .iter()
        .map(|name| {
            (
                (*name).into(),
                format!(
                    "{:x}",
                    Sha256::digest(std::fs::read(root.join(name)).unwrap())
                ),
            )
        })
        .collect()
}

#[test]
fn one_shot_migration_rejects_unverified_identity_without_runtime_or_source_writes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let original = root.join("original");
    let copied = root.join("copy");
    let storage = root.join("storage");
    for path in [&original, &copied, &storage] {
        std::fs::create_dir(path).unwrap();
    }
    let names = [
        "device.json",
        "crew-runtime.json",
        "native.jsonl",
        "scaffold-session.json",
    ];
    for path in [&original, &copied] {
        for (name, bytes) in names.iter().zip([
            "{\"deviceId\":\"preserved-host\"}",
            "{\"deploymentId\":null}",
            "{\"type\":\"session\",\"version\":3,\"id\":\"preserved-native\",\"cwd\":\"/repo\"}\n",
            "deliberately-invalid-controller-credential",
        ]) {
            std::fs::write(path.join(name), bytes).unwrap();
        }
    }
    let before_original = hashes(&original, &names);
    let before_copy = hashes(&copied, &names);
    let context = root.join("context.json");
    std::fs::write(&context, serde_json::to_vec(&serde_json::json!({
        "migrationId":"migration", "ownerSubject":"owner", "projectId":"ashler-staging", "deploymentId":"ashler-staging",
        "sourceWitnesses":{},
        "sourceOrigin":"https://comet-staging.internal.ashler.com", "targetOrigin":"https://comet-staging.internal.ashler.com",
        "sourceResource":"https://scaffold-staging.internal.ashler.com", "targetResource":"https://scaffold-staging.internal.ashler.com"
    })).unwrap()).unwrap();
    let context_before = std::fs::read(&context).unwrap();
    for mode in ["export-backup", "import-backup"] {
        let runtime = root.join(format!("unused-runtime-{mode}"));
        let destination = root.join(format!("destination-{mode}"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_zeron"));
        command
            .env_clear()
            .env("HOME", root)
            .env("TMPDIR", root)
            .arg("--data-dir")
            .arg(&runtime)
            .arg(mode)
            .arg("--destination")
            .arg(&destination)
            .arg("--identity-session")
            .arg(original.join("scaffold-session.json"))
            .arg("--source-context")
            .arg(&context);
        if mode == "export-backup" {
            command
                .arg("--source-profile-root")
                .arg(&original)
                .arg("--source-backup-root")
                .arg(&copied)
                .args(["--database-relative", "docs.sqlite3", "--writers-stopped"])
                .arg("--source-storage-directory")
                .arg(&storage)
                .arg("--native-history-root")
                .arg(&copied);
        } else {
            command.arg("--backup").arg(&copied).args([
                "--device-id",
                "preserved-host",
                "--validate-only",
            ]);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("Scaffold authorization rejected or expired")
        );
        assert!(output.stdout.is_empty());
        assert!(
            !runtime.exists(),
            "one-shot command must not create runtime settings, lock, device or stores"
        );
        assert!(!destination.exists());
        assert_eq!(hashes(&original, &names), before_original);
        assert_eq!(hashes(&copied, &names), before_copy);
        assert_eq!(std::fs::read(&context).unwrap(), context_before);
    }
}
