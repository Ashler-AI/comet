use std::path::{Path, PathBuf};

use loro::{ExportMode, LoroDoc, LoroValue, ToJson};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zeron_proto::CollaborationPrincipal;
use zeron_sync::crew_backup::export_legacy_crew_backup;

const CHAT: &str = "11111111-1111-4111-8111-111111111111";
const DATABASE: &str = "projects/ashler-staging/owner/docs.sqlite3";

fn fixture(root: &Path, owner: &str, chat_id: &str) -> (PathBuf, CollaborationPrincipal) {
    let source = root.join("copied-profile");
    std::fs::create_dir_all(source.join("projects/ashler-staging/owner")).unwrap();
    let principal = CollaborationPrincipal {
        subject: "owner".into(),
        email: None,
        project_id: "ashler-staging".into(),
        deployment_id: Some("ashler-staging".into()),
        session_id: None,
        capabilities: vec!["session.read".into()],
        unknown: Default::default(),
    };
    let workspace = LoroDoc::new();
    workspace
        .get_map("meta")
        .insert("schemaVersion", 3i64)
        .unwrap();
    workspace
        .get_map("chats")
        .insert(
            chat_id,
            LoroValue::from(json!({
                "id":chat_id, "deviceId":"host", "createdAt":1, "archived":false,
                "config":{"harness":"omp","model":"model-retained","sandbox":"read-only"},
                "harnessSessionId":"native-retained", "harnessSessionCwd":"/repo", "cwd":"/repo"
            })),
        )
        .unwrap();
    workspace
        .get_map("sessionRefs")
        .insert(
            &format!("5:owner:{chat_id}"),
            LoroValue::from(json!({
                "chatId":chat_id, "userId":"owner", "addedAt":1
            })),
        )
        .unwrap();
    let chat = LoroDoc::new();
    chat.get_map("meta").insert("chatId", chat_id).unwrap();
    chat.get_map("meta").insert("schemaVersion", 2i64).unwrap();
    chat.get_map("agentSessions").insert(chat_id, LoroValue::from(json!({
        "id":"owner-proof", "schemaVersion":2, "publishedAt":1, "publishedBy":owner,
        "kind":"agentSession", "value":{
            "sessionId":chat_id, "chatId":chat_id, "ownerSubject":owner, "ownerDeviceId":"host",
            "source":"local", "harness":"omp", "harnessSessionId":"native-retained", "createdAt":1
        }
    }))).unwrap();
    let db = Connection::open(source.join(DATABASE)).unwrap();
    db.execute_batch("CREATE TABLE snapshots(doc_id TEXT PRIMARY KEY,bytes BLOB); CREATE TABLE document_scopes(doc_id TEXT PRIMARY KEY,scope TEXT); CREATE TABLE processed_commands(command_id TEXT PRIMARY KEY,processed_at INTEGER);").unwrap();
    for (id, doc) in [("workspace3", workspace), (chat_id, chat)] {
        doc.commit();
        db.execute(
            "INSERT INTO snapshots VALUES (?1,?2)",
            params![id, doc.export(ExportMode::Snapshot).unwrap()],
        )
        .unwrap();
        db.execute("INSERT INTO document_scopes VALUES (?1,?2)", params![id, json!({
            "project":"ashler-staging", "principal":"owner", "deployment":"ashler-staging", "session":id
        }).to_string()]).unwrap();
    }
    db.execute(
        "INSERT INTO snapshots VALUES ('recovery-original/evidence', ?1)",
        [b"opaque recovery evidence".as_slice()],
    )
    .unwrap();
    drop(db);
    std::fs::create_dir_all(source.join("copied-omp/sessions/by-cwd")).unwrap();
    std::fs::create_dir_all(source.join("copied-omp/blobs")).unwrap();
    let blob = b"exact native image";
    let digest = format!("{:x}", Sha256::digest(blob));
    std::fs::write(source.join("copied-omp/blobs").join(&digest), blob).unwrap();
    let journal = format!(
        "{}\n{}\n",
        json!({"type":"session","version":3,"id":"native-retained","cwd":"/repo"}),
        json!({"type":"message","message":{"role":"user","content":[{"type":"image","data":format!("blob:sha256:{digest}")}]}})
    );
    std::fs::write(
        source.join("copied-omp/sessions/by-cwd/not-the-native-id.jsonl"),
        journal,
    )
    .unwrap();
    std::fs::create_dir_all(source.join("uploads/tmp")).unwrap();
    std::fs::create_dir_all(source.join("attachments")).unwrap();
    std::fs::write(source.join("uploads/retained.png"), b"upload bytes").unwrap();
    std::fs::write(source.join("attachments/retained.txt"), b"attachment bytes").unwrap();
    std::fs::write(source.join("uploads/tmp/unfinished"), b"not committed").unwrap();
    std::fs::write(
        source.join("session.json"),
        b"excluded authentication fixture",
    )
    .unwrap();
    std::fs::write(
        source.join("ui-settings.json"),
        b"{\"appearance\":\"dark\"}",
    )
    .unwrap();
    let journals = source
        .join(Path::new(DATABASE).parent().unwrap())
        .join("journals");
    std::fs::create_dir_all(&journals).unwrap();
    std::fs::write(
        journals.join("accepted.recovery"),
        b"retained recovery state",
    )
    .unwrap();
    (source, principal)
}

fn export(
    source: &Path,
    target: &Path,
    principal: &CollaborationPrincipal,
) -> Result<zeron_sync::crew_import::CrewBackupManifest, zeron_sync::crew_import::CrewImportError> {
    export_legacy_crew_backup(
        &target.parent().unwrap().join("original-profile"),
        source,
        Path::new(DATABASE),
        target,
        &[source.join("copied-omp")],
        principal,
    )
}

#[test]
fn exports_real_nested_database_native_header_blobs_and_exact_assets() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (source, principal) = fixture(&root, "owner", CHAT);
    let target = root.join("export");
    let manifest = export(&source, &target, &principal).unwrap();
    assert_eq!(manifest.chats[CHAT].owner_subject, "owner");
    assert_eq!(
        std::fs::read(source.join(DATABASE)).unwrap(),
        std::fs::read(target.join("docs.sqlite3")).unwrap()
    );
    let journal = &manifest.native_histories["native-retained"];
    assert_eq!(
        journal,
        "native-histories/0/sessions/by-cwd/not-the-native-id.jsonl"
    );
    assert_eq!(
        std::fs::read(source.join("copied-omp/sessions/by-cwd/not-the-native-id.jsonl")).unwrap(),
        std::fs::read(target.join(journal)).unwrap()
    );
    assert!(
        manifest
            .files
            .keys()
            .any(|path| path.starts_with("native-histories/0/blobs/"))
    );
    for path in [
        "uploads/retained.png",
        "attachments/retained.txt",
        "ui-settings.json",
    ] {
        assert_eq!(
            std::fs::read(source.join(path)).unwrap(),
            std::fs::read(target.join(path)).unwrap()
        );
    }
    assert_eq!(
        std::fs::read(target.join("legacy-journals/accepted.recovery")).unwrap(),
        b"retained recovery state"
    );
    assert!(!target.join("journals").exists());
    assert!(!target.join("session.json").exists());
    assert!(!target.join("uploads/tmp/unfinished").exists());
    for (path, digest) in &manifest.files {
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(std::fs::read(target.join(path)).unwrap())
            ),
            *digest
        );
    }
    let saved: Value =
        serde_json::from_slice(&std::fs::read(target.join("crew-backup.json")).unwrap()).unwrap();
    assert_eq!(saved["writerStopped"], true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for path in manifest
            .files
            .keys()
            .map(String::as_str)
            .chain(["crew-backup.json"])
        {
            assert_eq!(
                std::fs::metadata(target.join(path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    assert!(export(&source, &target, &principal).is_err());
}

#[test]
fn refuses_unproven_ownership_live_roots_wal_overlap_and_missing_native_evidence() {
    for scenario in [
        "foreign",
        "unproven",
        "live",
        "wal",
        "overlap",
        "missing",
        "duplicate",
        "corrupt-blob",
        "external-root",
        "non-omp",
        "scope",
        "secret-settings",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (source, principal) = fixture(
            &root,
            if scenario == "foreign" {
                "foreign"
            } else {
                "owner"
            },
            CHAT,
        );
        let target = if scenario == "overlap" {
            source.join("export")
        } else {
            root.join("export")
        };
        match scenario {
            "wal" => {
                std::fs::write(source.join(format!("{DATABASE}-wal")), b"uncheckpointed").unwrap()
            }
            "secret-settings" => std::fs::write(
                source.join("ui-settings.json"),
                br#"{"nested":{"accessToken":"excluded-fixture"}}"#,
            )
            .unwrap(),
            "missing" => std::fs::remove_file(
                source.join("copied-omp/sessions/by-cwd/not-the-native-id.jsonl"),
            )
            .unwrap(),
            "duplicate" => {
                std::fs::copy(
                    source.join("copied-omp/sessions/by-cwd/not-the-native-id.jsonl"),
                    source.join("copied-omp/sessions/duplicate.jsonl"),
                )
                .unwrap();
            }
            "corrupt-blob" => {
                let digest = format!("{:x}", Sha256::digest(b"exact native image"));
                std::fs::write(source.join("copied-omp/blobs").join(digest), b"corrupt").unwrap();
            }
            "unproven" | "non-omp" | "scope" => {
                let db = Connection::open(source.join(DATABASE)).unwrap();
                if scenario == "scope" {
                    db.execute("DELETE FROM document_scopes WHERE doc_id=?1", [CHAT])
                        .unwrap();
                } else {
                    let id = if scenario == "unproven" {
                        CHAT
                    } else {
                        "workspace3"
                    };
                    let bytes: Vec<u8> = db
                        .query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [id], |row| {
                            row.get(0)
                        })
                        .unwrap();
                    let doc = LoroDoc::new();
                    doc.import(&bytes).unwrap();
                    if scenario == "unproven" {
                        doc.get_map("agentSessions").delete(CHAT).unwrap();
                    } else {
                        doc.get_map("chats").insert(CHAT, LoroValue::from(json!({"id":CHAT,"deviceId":"host","config":{"harness":"codex"}}))).unwrap();
                    }
                    doc.commit();
                    db.execute(
                        "UPDATE snapshots SET bytes=?1 WHERE doc_id=?2",
                        params![doc.export(ExportMode::Snapshot).unwrap(), id],
                    )
                    .unwrap();
                }
            }
            _ => {}
        }
        let result = if scenario == "live" {
            export_legacy_crew_backup(
                &source,
                &source,
                Path::new(DATABASE),
                &target,
                &[source.join("copied-omp")],
                &principal,
            )
        } else if scenario == "external-root" {
            export_legacy_crew_backup(
                &root.join("original-profile"),
                &source,
                Path::new(DATABASE),
                &target,
                vec![root.clone()].as_slice(),
                &principal,
            )
        } else {
            export(&source, &target, &principal)
        };
        assert!(result.is_err(), "accepted {scenario}");
        assert!(
            !target.join("crew-backup.json").exists(),
            "published failed {scenario}"
        );
        assert!(source.join(DATABASE).exists());
    }
}

#[cfg(unix)]
#[test]
fn refuses_symlinks_and_hardlinks_even_inside_copied_profile() {
    for symlink in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (source, principal) = fixture(&root, "owner", CHAT);
        let asset = source.join("uploads/retained.png");
        let alias = source.join("attachments/alias.png");
        if symlink {
            std::os::unix::fs::symlink(&asset, alias).unwrap();
        } else {
            std::fs::hard_link(&asset, alias).unwrap();
        }
        let target = root.join("export");
        assert!(export(&source, &target, &principal).is_err());
        assert!(!target.exists());
    }
}

#[test]
fn preserves_local_native_identity_without_minting_cloud_identity() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let id = "local-chat-native-retained";
    let (source, principal) = fixture(&root, "owner", id);
    let target = root.join("export");
    let manifest = export(&source, &target, &principal).unwrap();
    assert_eq!(
        manifest
            .chats
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![id]
    );
    assert_eq!(manifest.chats[id].session_id, id);
    assert!(!zeron_proto::chat_allows_sync(id));
    assert_eq!(
        std::fs::read(source.join(DATABASE)).unwrap(),
        std::fs::read(target.join("docs.sqlite3")).unwrap()
    );
}

#[test]
fn reconciles_only_self_private_aliases_and_retains_exact_raw_database() {
    for self_alias in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (source, principal) = fixture(&root, "owner", CHAT);
        let alias = format!(
            "{CHAT}::session::{}",
            if self_alias {
                CHAT
            } else {
                "22222222-2222-4222-8222-222222222222"
            }
        );
        let private = LoroDoc::new();
        private
            .get_map("meta")
            .insert("schemaVersion", 2i64)
            .unwrap();
        private
            .get_map("meta")
            .insert("chatId", alias.as_str())
            .unwrap();
        private.commit();
        let db = Connection::open(source.join(DATABASE)).unwrap();
        db.execute(
            "INSERT INTO snapshots VALUES (?1,?2)",
            params![alias, private.export(ExportMode::Snapshot).unwrap()],
        )
        .unwrap();
        drop(db);
        let target = root.join("export");
        let result = export(&source, &target, &principal);
        if self_alias {
            let manifest = result.unwrap();
            assert_eq!(manifest.chats.len(), 1);
            assert!(manifest.chats.contains_key(CHAT));
            assert!(!manifest.chats.contains_key(&alias));
            assert_eq!(
                std::fs::read(source.join(DATABASE)).unwrap(),
                std::fs::read(target.join("docs.sqlite3")).unwrap()
            );
        } else {
            assert!(result.is_err());
            assert!(!target.exists());
        }
    }
}

#[test]
fn remote_export_requires_exact_fenced_context_and_preserves_copied_bytes() {
    use base64::Engine;
    use zeron_sync::crew_remote_backup::{RemoteBackupContext, export_remote_crew_backup};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (source, principal) = fixture(&root, "owner", CHAT);
    let context = RemoteBackupContext {
        migration_id: "migration".into(),
        owner_subject: "owner".into(),
        project_id: "ashler-staging".into(),
        deployment_id: "ashler-staging".into(),
        logical_session_id: CHAT.into(),
        sandbox_id: "sandbox".into(),
        device_id: "host".into(),
        lifecycle_epoch: 7,
        source_generation: "session".into(),
        writers_stopped: true,
    };
    let db = Connection::open(source.join(DATABASE)).unwrap();
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT bytes FROM snapshots WHERE doc_id=?1",
            [CHAT],
            |row| row.get(0),
        )
        .unwrap();
    let doc = LoroDoc::new();
    doc.import(&bytes).unwrap();
    doc.get_map("agentSessions").insert(CHAT, LoroValue::from(json!({
        "id":"remote-owner-proof", "schemaVersion":2, "publishedAt":1, "publishedBy":"owner",
        "kind":"agentSession", "value":{
            "sessionId":CHAT, "chatId":CHAT, "ownerSubject":"owner", "ownerDeviceId":"host",
            "source":"scaffold", "harness":"omp", "harnessSessionId":"native-retained", "createdAt":1,
            "environment":{"ownerPrincipal":"owner", "scope":{"projectId":"ashler-staging","deploymentId":"ashler-staging","sessionId":CHAT},
                "source":{"kind":"scaffold","sandbox_id":"sandbox","lifecycle":"ready","lifecycle_epoch":7}}
        }
    }))).unwrap();
    doc.commit();
    db.execute(
        "UPDATE snapshots SET bytes=?1 WHERE doc_id=?2",
        params![doc.export(ExportMode::Snapshot).unwrap(), CHAT],
    )
    .unwrap();
    drop(db);
    let baseline = doc.export(ExportMode::Snapshot).unwrap();
    let baseline_version = doc.oplog_vv();
    doc.get_map("meta")
        .insert("edgeOnly", "retained authoritative operation")
        .unwrap();
    let remote_body = b"R2-only attachment";
    let remote_hash = format!("{:x}", Sha256::digest(remote_body));
    let remote_alias = root
        .join("original-profile/uploads/remote.png")
        .to_string_lossy()
        .into_owned();
    doc.get_list("publications").push(LoroValue::from(json!({"record":{
        "id":"remote-attachment", "schemaVersion":2, "publishedAt":1, "publishedBy":"owner",
        "kind":"attachment", "value":{"id":"remote-attachment", "fileName":"remote.png", "mimeType":"image/png",
            "sizeBytes":remote_body.len(), "blobKey":format!("/attachments/{remote_hash}"), "sha256":remote_hash,
            "createdBy":"owner", "createdAt":1, "metadata":{"localPath":remote_alias}}
    }}))).unwrap();
    doc.commit();
    let edge_storage = root.join("edge-storage.json");
    let edge_bytes = serde_json::to_vec(&json!({
        "schemaVersion":1,"format":"legacy-session-sql-v1","sourceGeneration":"session","migrationId":"migration",
        "scope":{"ownerSubject":"owner","projectId":"ashler-staging","deploymentId":"ashler-staging","logicalSessionId":CHAT},
        "frozenAt":"2026-10-21T00:00:00Z", "meta":[],
        "blobs":[{"name":"snapshot","idx":0,"bytesBase64":base64::engine::general_purpose::STANDARD.encode(&baseline)}],
        "updates":[{"seq":7,"receivedAt":1,"bytesBase64":base64::engine::general_purpose::STANDARD.encode(doc.export(ExportMode::updates(&baseline_version)).unwrap())}]
    })).unwrap();
    std::fs::write(&edge_storage, &edge_bytes).unwrap();
    assert!(export(&source, &root.join("local-rejected"), &principal).is_err());
    let mut changed = context.clone();
    changed.lifecycle_epoch = 8;
    assert!(
        export_remote_crew_backup(
            &root.join("original-profile"),
            &source,
            Path::new(DATABASE),
            &root.join("wrong-epoch"),
            &[source.join("copied-omp")],
            &edge_storage,
            None,
            &changed
        )
        .is_err()
    );
    assert!(!root.join("wrong-epoch").exists());
    let target = root.join("remote-export");
    let native_hash = format!("{:x}", Sha256::digest(b"exact native image"));
    std::fs::remove_file(source.join("copied-omp/blobs").join(&native_hash)).unwrap();
    let inventory = zeron_sync::crew_remote_backup::inventory_remote_crew_backup(
        &root.join("original-profile"),
        &source,
        Path::new(DATABASE),
        &target,
        &[source.join("copied-omp")],
        &edge_storage,
        None,
        &context,
    )
    .unwrap();
    assert!(inventory.missing_attachments.contains(&remote_hash));
    assert!(inventory.missing_attachments.contains(&native_hash));
    assert!(
        inventory
            .native_missing_blobs
            .iter()
            .any(|blob| blob.sha256 == native_hash)
    );
    assert!(inventory.unresolved_references.is_empty());
    assert!(
        !target.exists(),
        "inventory cannot publish an importer-ready backup"
    );
    let fetched = root.join("fetched");
    std::fs::create_dir(&fetched).unwrap();
    std::fs::write(fetched.join(&native_hash), b"exact native image").unwrap();
    std::fs::write(fetched.join(&remote_hash), b"corrupt fetched bytes").unwrap();
    assert!(
        export_remote_crew_backup(
            &root.join("original-profile"),
            &source,
            Path::new(DATABASE),
            &target,
            &[source.join("copied-omp")],
            &edge_storage,
            Some(&fetched),
            &context
        )
        .is_err()
    );
    assert!(!target.exists());
    std::fs::write(fetched.join(&remote_hash), remote_body).unwrap();
    let manifest = export_remote_crew_backup(
        &root.join("original-profile"),
        &source,
        Path::new(DATABASE),
        &target,
        &[source.join("copied-omp")],
        &edge_storage,
        Some(&fetched),
        &context,
    )
    .unwrap();
    context.manifest(&manifest).unwrap();
    assert_eq!(
        std::fs::read(source.join(DATABASE)).unwrap(),
        std::fs::read(target.join("source-evidence/host.sqlite3")).unwrap()
    );
    assert_eq!(
        edge_bytes,
        std::fs::read(target.join("source-evidence/edge-storage.json")).unwrap()
    );
    let merged_db = Connection::open(target.join("docs.sqlite3")).unwrap();
    let merged_bytes: Vec<u8> = merged_db
        .query_row(
            "SELECT bytes FROM snapshots WHERE doc_id=?1",
            [CHAT],
            |row| row.get(0),
        )
        .unwrap();
    let merged = LoroDoc::new();
    merged.import(&merged_bytes).unwrap();
    assert_eq!(
        merged.oplog_vv(),
        doc.oplog_vv(),
        "Edge-only operations survive the host reconciliation"
    );
    assert!(manifest.native_histories.contains_key("native-retained"));
    assert!(manifest.files.contains_key("ui-settings.json"));
    assert!(manifest.files.contains_key("uploads/retained.png"));
    assert_eq!(
        std::fs::read(target.join("uploads/remote.png")).unwrap(),
        remote_body
    );
    assert_eq!(
        manifest.attachment_aliases[&remote_alias],
        "uploads/remote.png"
    );
}

#[test]
fn desktop_preparation_preserves_original_null_scope_and_all_raw_sources() {
    use base64::Engine;
    use zeron_sync::crew_desktop_backup::DesktopSourceContext;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (source, principal) = fixture(&root, "owner", CHAT);
    let mut context = DesktopSourceContext {
        migration_id: "desktop-migration".into(),
        owner_subject: "owner".into(),
        project_id: "ashler-staging".into(),
        deployment_id: "ashler-staging".into(),
        source_origin: "https://comet-staging.internal.ashler.com".into(),
        target_origin: "https://comet-staging.internal.ashler.com".into(),
        source_resource: "https://scaffold-staging.internal.ashler.com".into(),
        target_resource: "https://scaffold-staging.internal.ashler.com".into(),
        source_witnesses: Default::default(),
    };
    let db = Connection::open(source.join(DATABASE)).unwrap();
    db.execute(
        "UPDATE document_scopes SET scope=?1 WHERE doc_id=?2",
        params![
            json!({
                "project":"ashler-staging","principal":"owner","deployment":null,"session":CHAT
            })
            .to_string(),
            CHAT
        ],
    )
    .unwrap();
    let storage = root.join("frozen");
    std::fs::create_dir_all(storage.join("sessions")).unwrap();
    for (id, generation, scope, path) in [
        (
            "workspace3",
            "workspace",
            json!({"projectId":"ashler-staging","workspaceId":"ws4/ashler-staging",
            "authority":"project","exportedBySubject":"owner"}),
            storage.join("workspace.json"),
        ),
        (
            CHAT,
            "session",
            json!({"ownerSubject":"owner","projectId":"ashler-staging",
            "deploymentId":null,"logicalSessionId":CHAT}),
            storage.join(format!("sessions/{CHAT}.json")),
        ),
    ] {
        let snapshot: Vec<u8> = db
            .query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        let raw = serde_json::to_vec(&json!({
            "schemaVersion":1,"format":"legacy-session-sql-v1","sourceGeneration":generation,
            "migrationId":"desktop-migration","scope":scope,"frozenAt":"2026-10-21T00:00:00Z","meta":[],
            "blobs":[{"name":"snapshot","idx":0,"bytesBase64":base64::engine::general_purpose::STANDARD.encode(snapshot)}],
            "updates":[]
        })).unwrap();
        context.source_witnesses.insert(
            id.into(),
            zeron_sync::crew_desktop_backup::DesktopDocumentWitness {
                migration_id: "desktop-migration".into(),
                sha256: format!("{:x}", Sha256::digest(&raw)),
            },
        );
        std::fs::write(path, raw).unwrap();
    }
    drop(db);
    let target = root.join("desktop-backup");
    let manifest = zeron_sync::crew_backup::export_desktop_crew_backup(
        &root.join("original-profile"),
        &source,
        Path::new(DATABASE),
        &target,
        &[source.join("copied-omp")],
        &principal,
        &context,
        &storage,
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        manifest.chats[CHAT].scope.deployment_id.as_deref(),
        Some("ashler-staging")
    );
    let proof = manifest.desktop_source.as_ref().unwrap();
    assert!(proof.document_scopes[CHAT]["deployment"].is_null());
    proof
        .validate_evidence(&target, &manifest.files, &principal)
        .unwrap();
    let db = Connection::open(target.join("docs.sqlite3")).unwrap();
    let retained: String = db
        .query_row(
            "SELECT scope FROM document_scopes WHERE doc_id=?1",
            [CHAT],
            |row| row.get(0),
        )
        .unwrap();
    assert!(serde_json::from_str::<Value>(&retained).unwrap()["deployment"].is_null());
    assert_eq!(
        std::fs::read(source.join(DATABASE)).unwrap(),
        std::fs::read(target.join("source-evidence/desktop/docs.sqlite3")).unwrap()
    );
}

#[test]
fn coordinator_original_epoch_authority_exports_and_verifies_inert_history_without_fake_journal() {
    use base64::Engine;
    use std::collections::BTreeMap;
    use zeron_sync::crew_desktop_backup::{DesktopDocumentWitness, DesktopSourceContext};
    use zeron_sync::crew_import::{
        OriginalEpochProof, import_legacy_crew_backup, imported_original_epoch_histories,
        imported_original_epoch_proof_json, verify_imported_profile, verify_legacy_crew_import,
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (source, principal) = fixture(&root, "owner", CHAT);
    // This fixture models an already absent original guest, not a newly killed VM.
    std::fs::remove_dir_all(source.join("copied-omp")).unwrap();
    std::fs::remove_file(
        source
            .join(Path::new(DATABASE).parent().unwrap())
            .join("journals/accepted.recovery"),
    )
    .unwrap();
    let db = Connection::open(source.join(DATABASE)).unwrap();
    let workspace_bytes: Vec<u8> = db
        .query_row(
            "SELECT bytes FROM snapshots WHERE doc_id='workspace3'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let workspace = LoroDoc::new();
    workspace.import(&workspace_bytes).unwrap();
    let mut original_chat = workspace.get_deep_value().to_json_value()["chats"][CHAT].clone();
    original_chat["deviceId"] = json!("comet-scaffold-original-sandbox");
    workspace
        .get_map("chats")
        .insert(CHAT, LoroValue::from(original_chat))
        .unwrap();
    workspace.commit();
    db.execute(
        "UPDATE snapshots SET bytes=?1 WHERE doc_id='workspace3'",
        [workspace.export(ExportMode::Snapshot).unwrap()],
    )
    .unwrap();
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT bytes FROM snapshots WHERE doc_id=?1",
            [CHAT],
            |row| row.get(0),
        )
        .unwrap();
    let chat = LoroDoc::new();
    chat.import(&bytes).unwrap();
    chat.get_map("agentSessions").insert(CHAT, LoroValue::from(json!({
        "id":"original-owner", "schemaVersion":2, "publishedAt":1, "publishedBy":"owner", "kind":"agentSession",
        "value":{"sessionId":CHAT,"chatId":CHAT,"ownerSubject":"owner","ownerDeviceId":"comet-scaffold-original-sandbox", "source":"scaffold",
            "harness":"omp","harnessSessionId":"native-retained","createdAt":1,
            "environment":{"ownerPrincipal":"owner","scope":{"projectId":"ashler-staging","deploymentId":"ashler-staging","sessionId":CHAT},
                "source":{"kind":"scaffold","sandbox_id":"original-sandbox","lifecycle":"stopped","lifecycle_epoch":7}}}
    }))).unwrap();
    let mut expected_outcomes = BTreeMap::new();
    for status in [
        "pending",
        "claimed",
        "applied",
        "rejected",
        "cancelled",
        "unknown",
    ] {
        let id = format!("original-{status}");
        expected_outcomes.insert(id.clone(), status.to_owned());
        let command = chat
            .get_list("commands")
            .push_container(loro::LoroMap::new())
            .unwrap();
        for (key, value) in json!({"id":id,"kind":"interrupt","payload":{"kind":"interrupt"},"issuedBy":"host",
            "issuedAt":12,"basedOn":{"turnId":"original-turn","frontier":null},"expiresAt":1000,"status":status,
            "resolution":"original source outcome"}).as_object().unwrap() {
            command.insert(key, LoroValue::from(value.clone())).unwrap();
        }
    }
    let publication = chat
        .get_list("publications")
        .push_container(loro::LoroMap::new())
        .unwrap();
    publication.insert("id", "original-comment").unwrap();
    publication.insert("record", LoroValue::from(json!({"id":"original-comment","schemaVersion":2,"publishedAt":1,
        "publishedBy":"owner","kind":"annotation","value":{"id":"original-comment","authorSubject":"owner",
            "body":"Original comment retained","anchor":{"targetKind":"message","targetId":"original-message"},
            "state":"orphaned","createdAt":1}}))).unwrap();
    chat.commit();
    db.execute(
        "UPDATE snapshots SET bytes=?1 WHERE doc_id=?2",
        params![chat.export(ExportMode::Snapshot).unwrap(), CHAT],
    )
    .unwrap();
    db.execute(
        "INSERT INTO processed_commands VALUES ('unbound-historical-claim',1)",
        [],
    )
    .unwrap();
    let mut context = DesktopSourceContext {
        migration_id: "desktop-batch".into(),
        owner_subject: "owner".into(),
        project_id: "ashler-staging".into(),
        deployment_id: "ashler-staging".into(),
        source_origin: "https://comet-staging.internal.ashler.com".into(),
        target_origin: "https://comet-staging.internal.ashler.com".into(),
        source_resource: "https://scaffold-staging.internal.ashler.com".into(),
        target_resource: "https://scaffold-staging.internal.ashler.com".into(),
        source_witnesses: BTreeMap::new(),
    };
    let frozen = root.join("frozen-original");
    std::fs::create_dir_all(frozen.join("sessions")).unwrap();
    for (id, generation, migration, scope, path) in [
        (
            "workspace3",
            "workspace",
            "desktop-batch",
            json!({"projectId":"ashler-staging","workspaceId":"ws4/ashler-staging",
            "authority":"project","exportedBySubject":"owner"}),
            frozen.join("workspace.json"),
        ),
        (
            CHAT,
            "session",
            "server-session-claim",
            json!({"ownerSubject":"owner","projectId":"ashler-staging",
            "deploymentId":"ashler-staging","logicalSessionId":CHAT}),
            frozen.join(format!("sessions/{CHAT}.json")),
        ),
    ] {
        let snapshot: Vec<u8> = db
            .query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        let raw = serde_json::to_vec(&json!({"schemaVersion":1,"format":"legacy-session-sql-v1","sourceGeneration":generation,
            "migrationId":migration,"scope":scope,"frozenAt":"2026-10-21T00:00:00Z","meta":[],
            "blobs":[{"name":"snapshot","idx":0,"bytesBase64":base64::engine::general_purpose::STANDARD.encode(snapshot)}],"updates":[]})).unwrap();
        context.source_witnesses.insert(
            id.into(),
            DesktopDocumentWitness {
                migration_id: migration.into(),
                sha256: format!("{:x}", Sha256::digest(&raw)),
            },
        );
        std::fs::write(path, raw).unwrap();
    }
    drop(db);
    let authority_root = root.join("coordinator-authority");
    std::fs::create_dir_all(authority_root.join("sessions")).unwrap();
    let authority_path = authority_root.join(format!("sessions/{CHAT}.json"));
    // Explicit coordinator-response fixture. The producer must derive the final
    // native association/outcome digest; this response contains neither marker.
    let authority = json!({"version":1,"kind":"terminal-source-authority","migrationId":"server-session-claim",
        "scope":{"ownerSubject":"owner","projectId":"ashler-staging","deploymentId":"ashler-staging","logicalSessionId":CHAT,
            "sandboxId":"original-sandbox","deviceId":"comet-scaffold-original-sandbox","lifecycleEpoch":7},
        "coordinatorLifecycleEpoch":8,
        "sourceSha256":context.source_witnesses[CHAT].sha256,
        "terminal":{"status":"stopped","providerAction":"kill","terminatedAt":"2026-10-20T00:00:00Z",
            "terminationReason":"timeout","physicalSandboxId":"original-sandbox","providerNotFoundObservedAt":"2026-10-21T00:00:00Z",
            "retainedSnapshotIds":[]}});
    let export = |target: &Path, authorities: Option<&Path>| {
        zeron_sync::crew_backup::export_desktop_crew_backup(
            &root.join("original-profile"),
            &source,
            Path::new(DATABASE),
            target,
            &[],
            &principal,
            &context,
            &frozen,
            None,
            authorities,
        )
    };
    assert!(export(&root.join("no-authority"), None).is_err());
    for invalid in [
        "epoch",
        "hash",
        "snapshots",
        "native-injection",
        "observation",
    ] {
        let mut bad = authority.clone();
        match invalid {
            "epoch" => bad["scope"]["lifecycleEpoch"] = json!(8),
            "hash" => bad["sourceSha256"] = json!("0".repeat(64)),
            "snapshots" => bad["terminal"]["retainedSnapshotIds"] = json!(["still-resumable"]),
            "native-injection" => bad["nativeSessionId"] = json!("forged-native"),
            "observation" => {
                bad["terminal"]["providerNotFoundObservedAt"] = json!("2026-10-19T00:00:00Z")
            }
            _ => unreachable!(),
        }
        std::fs::write(&authority_path, serde_json::to_vec(&bad).unwrap()).unwrap();
        let target = root.join(format!("rejected-{invalid}"));
        assert!(
            export(&target, Some(&authority_root)).is_err(),
            "accepted {invalid}"
        );
        assert!(!target.exists());
    }
    let authority_bytes = serde_json::to_vec(&authority).unwrap();
    std::fs::write(&authority_path, &authority_bytes).unwrap();
    let backup = root.join("terminal-backup");
    let manifest = export(&backup, Some(&authority_root)).unwrap();
    assert!(manifest.native_histories.is_empty());
    assert_eq!(
        manifest.original_epoch_histories[CHAT]
            .native_session_id
            .as_deref(),
        Some("native-retained")
    );
    assert_eq!(
        manifest.chats[CHAT].original_epoch_history.as_ref(),
        manifest.original_epoch_histories.get(CHAT)
    );
    assert_eq!(
        std::fs::read(backup.join(format!("source-evidence/terminal-authority/{CHAT}.json")))
            .unwrap(),
        authority_bytes
    );
    assert_eq!(
        std::fs::read(backup.join(format!("source-evidence/desktop/sessions/{CHAT}.json")))
            .unwrap(),
        std::fs::read(frozen.join(format!("sessions/{CHAT}.json"))).unwrap()
    );
    let destination = root.join("native-controller-profile");
    let imported =
        import_legacy_crew_backup(&backup, &destination, &principal, "controller").unwrap();
    assert_eq!(imported.native_session_ids[CHAT], "native-retained");
    assert!(imported.native_histories.is_empty());
    assert_eq!(imported.annotations_retained, 1);
    let readback =
        verify_legacy_crew_import(&backup, &destination, &principal, "controller").unwrap();
    assert_eq!(readback.command_outcomes["original-pending"], "pending");
    assert_eq!(
        readback.command_outcomes["unbound-historical-claim"],
        "unknown"
    );
    let selected = verify_imported_profile(&destination, &principal, "controller", CHAT).unwrap();
    assert_eq!(selected.command_outcomes, expected_outcomes);
    assert_eq!(selected.native_session_ids[CHAT], "native-retained");
    assert!(selected.native_histories.is_empty());
    assert_eq!(
        imported_original_epoch_histories(&destination).unwrap(),
        manifest.original_epoch_histories
    );
    let proof_json = imported_original_epoch_proof_json(&destination, &selected)
        .unwrap()
        .unwrap();
    let proof: OriginalEpochProof = serde_json::from_str(&proof_json).unwrap();
    assert_eq!(proof.migration_id, "server-session-claim");
    assert_eq!(proof.native_session_id.as_deref(), Some("native-retained"));
    assert_eq!(
        proof.command_outcomes_sha256,
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&expected_outcomes).unwrap())
        )
    );
    let db = Connection::open(destination.join("docs.sqlite3")).unwrap();
    for id in expected_outcomes.keys() {
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM processed_commands WHERE command_id=?1",
                [id],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
}

#[test]
fn inert_bootstrap_inspection_and_export_preserve_evidence_and_reject_associations() {
    const ORPHAN: &str = "22222222-2222-4222-8222-222222222222";
    for scenario in [
        "inert",
        "history",
        "membership",
        "native",
        "scope",
        "opaque-recovery",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (source, principal) = fixture(&root, "owner", CHAT);
        let db = Connection::open(source.join(DATABASE)).unwrap();
        if scenario != "opaque-recovery" {
            db.execute(
                "DELETE FROM snapshots WHERE doc_id='recovery-original/evidence'",
                [],
            )
            .unwrap();
        }
        let orphan = LoroDoc::new();
        orphan.get_map("meta").insert("chatId", ORPHAN).unwrap();
        orphan
            .get_map("meta")
            .insert("schemaVersion", 2i64)
            .unwrap();
        orphan
            .get_map("meta")
            .insert("directoryCompletedTurn", "")
            .unwrap();
        if scenario == "history" {
            orphan
                .get_map("meta")
                .insert("meaningful", "accepted prior state")
                .unwrap();
            orphan.commit();
            orphan.get_map("meta").delete("meaningful").unwrap();
        }
        orphan.commit();
        let bytes = orphan.export(ExportMode::Snapshot).unwrap();
        db.execute(
            "INSERT INTO snapshots VALUES (?1,?2)",
            params![ORPHAN, bytes],
        )
        .unwrap();
        if scenario == "scope" {
            db.execute("INSERT INTO document_scopes VALUES (?1,?2)", params![ORPHAN, json!({
                "project":"ashler-staging","principal":"owner","deployment":"ashler-staging","session":ORPHAN
            }).to_string()]).unwrap();
        }
        if scenario == "membership" {
            let bytes: Vec<u8> = db
                .query_row(
                    "SELECT bytes FROM snapshots WHERE doc_id='workspace3'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let workspace = LoroDoc::new();
            workspace.import(&bytes).unwrap();
            workspace
                .get_map("sessionRefs")
                .insert(
                    &format!("5:owner:{ORPHAN}"),
                    LoroValue::from(json!({
                        "chatId":ORPHAN,"userId":"owner","addedAt":1
                    })),
                )
                .unwrap();
            workspace.commit();
            db.execute(
                "UPDATE snapshots SET bytes=?1 WHERE doc_id='workspace3'",
                [workspace.export(ExportMode::Snapshot).unwrap()],
            )
            .unwrap();
        }
        drop(db);
        if scenario == "native" {
            std::fs::write(
                source.join("copied-omp/sessions/by-cwd/unrelated-filename.jsonl"),
                format!(
                    "{}\n",
                    json!({"type":"session","version":3,"id":ORPHAN,"cwd":"/orphan"})
                ),
            )
            .unwrap();
        }
        let before = std::fs::read(source.join(DATABASE)).unwrap();
        let inspection = zeron_sync::crew_backup::inspect_inert_bootstraps(
            &source,
            Path::new(DATABASE),
            &[source.join("copied-omp")],
        )
        .unwrap();
        assert_eq!(
            inspection["sourceDatabaseSha256"],
            format!("{:x}", Sha256::digest(&before))
        );
        assert_eq!(
            std::fs::read(source.join(DATABASE)).unwrap(),
            before,
            "inspection must not write SQLite"
        );
        let target = root.join("export");
        let result = export(&source, &target, &principal);
        if scenario == "inert" {
            let manifest = result.unwrap();
            assert_eq!(
                inspection["inertBootstrapSnapshots"][ORPHAN]["disposition"],
                "inert-bootstrap"
            );
            assert_eq!(
                inspection["inertBootstrapSnapshots"][ORPHAN]["snapshotSha256"],
                format!("{:x}", Sha256::digest(&bytes))
            );
            assert!(!manifest.chats.contains_key(ORPHAN));
            assert_eq!(std::fs::read(target.join("docs.sqlite3")).unwrap(), before);
        } else {
            assert!(
                inspection["inertBootstrapSnapshots"].get(ORPHAN).is_none(),
                "classified associated {scenario}"
            );
            assert!(result.is_err(), "silently omitted associated {scenario}");
            assert!(!target.exists());
        }
    }
}

#[test]
fn streamed_metadata_matches_single_session_and_gates_late_invalid_documents() {
    for scenario in ["valid", "late-owner", "late-malformed"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (source, principal) = fixture(&root, "owner", CHAT);
        let baseline = export(&source, &root.join("baseline"), &principal).unwrap();
        let db = Connection::open(source.join(DATABASE)).unwrap();
        let bytes: Vec<u8> = db.query_row("SELECT bytes FROM snapshots WHERE doc_id='workspace3'", [], |row| row.get(0)).unwrap();
        let workspace = LoroDoc::new();
        workspace.import(&bytes).unwrap();
        let original = workspace.get_deep_value().to_json_value();
        let mut expected_chats = baseline.chats.clone();
        for number in 2..=9 {
            let id = format!("{number:08}-2222-4222-8222-222222222222");
            let mut row = original["chats"][CHAT].clone();
            row["id"] = json!(id);
            workspace.get_map("chats").insert(&id, LoroValue::from(row)).unwrap();
            workspace.get_map("sessionRefs").insert(&format!("5:owner:{id}"), LoroValue::from(json!({
                "chatId":id,"userId":"owner","addedAt":1
            }))).unwrap();
            let doc = LoroDoc::new();
            doc.get_map("meta").insert("chatId", id.as_str()).unwrap();
            doc.get_map("meta").insert("schemaVersion", 2i64).unwrap();
            let owner = if number == 9 && scenario == "late-owner" { "foreign" } else { "owner" };
            doc.get_map("agentSessions").insert(&id, LoroValue::from(json!({
                "id":"owner-proof","schemaVersion":2,"publishedAt":1,"publishedBy":owner,
                "kind":"agentSession","value":{
                    "sessionId":id,"chatId":id,"ownerSubject":owner,"ownerDeviceId":"host",
                    "source":"local","harness":"omp","harnessSessionId":"native-retained","createdAt":1
                }
            }))).unwrap();
            doc.get_list("messages").push(LoroValue::from(json!({
                "id":format!("message-{number}"),"role":"assistant","status":"complete",
                "parts":[{"id":"text","type":"text","text":"retained history ".repeat(2048)}]
            }))).unwrap();
            doc.commit();
            let snapshot = if number == 9 && scenario == "late-malformed" {
                b"invalid final source document".to_vec()
            } else { doc.export(ExportMode::Snapshot).unwrap() };
            db.execute("INSERT INTO snapshots VALUES (?1,?2)", params![id, snapshot]).unwrap();
            db.execute("INSERT INTO document_scopes VALUES (?1,?2)", params![id, json!({
                "project":"ashler-staging","principal":"owner","deployment":"ashler-staging","session":id
            }).to_string()]).unwrap();
            let mut expected = baseline.chats[CHAT].clone();
            expected.session_id = id.clone();
            expected.scope.session_id = Some(id.clone());
            expected_chats.insert(id, expected);
        }
        workspace.commit();
        db.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id='workspace3'", [workspace.export(ExportMode::Snapshot).unwrap()]).unwrap();
        drop(db);
        let before = std::fs::read(source.join(DATABASE)).unwrap();
        let destination = root.join("streamed");
        let result = export(&source, &destination, &principal);
        assert_eq!(std::fs::read(source.join(DATABASE)).unwrap(), before);
        if scenario == "valid" {
            let manifest = result.unwrap();
            assert_eq!(manifest.chats, expected_chats);
            assert_eq!(manifest.native_histories, baseline.native_histories);
            assert_eq!(std::fs::read(destination.join("docs.sqlite3")).unwrap(), before);
            for (path, digest) in baseline.files.iter().filter(|(path, _)| path.as_str() != "docs.sqlite3") {
                assert_eq!(manifest.files.get(path), Some(digest));
            }
        } else {
            assert!(result.is_err(), "accepted {scenario} after valid earlier histories");
            assert!(!destination.exists(), "published destination before all source gates completed");
        }
    }
}
