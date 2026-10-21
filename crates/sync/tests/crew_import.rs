use std::collections::BTreeMap;
use loro::{ExportMode, LoroDoc, LoroValue};
use sha2::{Digest, Sha256};
use zeron_proto::{CollaborationPrincipal, CollaborationScope, CrewChatMetadata};
use zeron_sync::{DocsStore, crew_import::{CrewBackupManifest, import_legacy_crew_backup}};

const CHAT: &str = "11111111-1111-4111-8111-111111111111";

// Wire shapes from committed Crew 297717c5, not the upstream serializers.
// In particular tool resolution is isError presence, and input IDs are request IDs.
fn legacy_messages() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"id":"user-1","role":"user","createdAt":10,"deviceId":"host",
            "status":"complete","parts":[{"id":"user-text","kind":"text","text":"Inspect the image.\n\nAttached images (local files):\n/original/profile/uploads/retained.png"}]}),
        serde_json::json!({"id":"assistant-1","role":"assistant","createdAt":11,"deviceId":"host",
            "status":"complete","parts":[
                {"id":"text-1","kind":"text","text":"Résumé — image inspected."},
                {"id":"exec-1","kind":"tool","call":{"kind":"exec","command":"printf 'fixture only'"},"isError":false},
                {"id":"todo-1","kind":"tool","call":{"kind":"todo","items":[{"text":"Inspect image","done":true}]},"isError":false},
                {"id":"goal-1","kind":"tool","call":{"kind":"unknown","name":"omp_goal_state","input":{"goal":{"id":"goal-stable","objective":"Preserve history","status":"active"}}}},
                {"id":"request-1","kind":"input","questions":[{"id":"question-1","header":"Continue","question":"Keep history?","options":["Yes"],"multiSelect":false}],"resolved":true},
                {"id":"error-1","kind":"error","message":"Retained historical warning"},
                {"id":"thinking-1","kind":"thinking","started":true,"completed":true},
                {"id":"agent-1","kind":"tool","call":{"kind":"agent","agents":[
                    {"id":"Scout","role":"scout","status":"completed","model":"fixture-model"},
                    {"id":"Review","role":"reviewer","status":"failed"}
                ]},"isError":true}
            ]}),
        serde_json::json!({"id":"assistant-1#c1","role":"assistant","createdAt":11,"deviceId":"host",
            "status":"complete","continuationOf":"assistant-1","parts":[{"id":"text-2","kind":"text","text":"Continuation retained."}]}),
    ]
}

fn legacy_command(id: &str, status: &str) -> serde_json::Value {
    serde_json::json!({"id":id,"kind":"interrupt","payload":{"kind":"interrupt"},
        "issuedBy":"host","issuedAt":12,"basedOn":{"turnId":"assistant-1","frontier":null},
        "expiresAt":1000,"status":status,"resolution":"retained"})
}

fn legacy_annotation(body: &str, state: &str, resolved_at: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"id":"annotation-1","authorSubject":"owner","body":body,
        "anchor":{"targetKind":"message","targetId":"assistant-1","byteRange":{"start":0,"end":8},"exact":"Résumé",
            "futureAnchorField":"preserve"},
        "state":state,"createdAt":13,"resolvedAt":resolved_at,"futureAnnotationField":{"retained":true}})
}

fn push_row(doc: &LoroDoc, container: &str, value: &serde_json::Value) {
    let row = doc.get_list(container).push_container(loro::LoroMap::new()).unwrap();
    for (key, value) in value.as_object().unwrap() {
        if key == "parts" {
            let parts = row.insert_container(key, loro::LoroList::new()).unwrap();
            for value in value.as_array().unwrap() {
                let part = parts.push_container(loro::LoroMap::new()).unwrap();
                for (key, value) in value.as_object().unwrap() {
                    if key == "text" {
                        part.insert_container(key, loro::LoroText::new()).unwrap().insert(0, value.as_str().unwrap()).unwrap();
                    } else if key == "call" {
                        let call = part.insert_container(key, loro::LoroMap::new()).unwrap();
                        for (key, value) in value.as_object().unwrap() {
                            call.insert(key, LoroValue::from(value.clone())).unwrap();
                        }
                    } else {
                        part.insert(key, LoroValue::from(value.clone())).unwrap();
                    }
                }
            }
        } else {
            row.insert(key, LoroValue::from(value.clone())).unwrap();
        }
    }
}

fn refresh_database_hash(path: &std::path::Path) {
    let manifest_path = path.join("crew-backup.json");
    let mut manifest: CrewBackupManifest = serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest.files.insert("docs.sqlite3".into(), format!("{:x}", Sha256::digest(std::fs::read(path.join("docs.sqlite3")).unwrap())));
    std::fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
}

fn backup(path: &std::path::Path, pending: bool) -> CollaborationPrincipal {
    let principal = CollaborationPrincipal { subject: "owner".into(), email: None,
        project_id: "project".into(), deployment_id: Some("canary".into()),
        session_id: None, capabilities: vec!["session.read".into()], unknown: Default::default() };
    let scope = CollaborationScope { project_id: principal.project_id.clone(),
        deployment_id: principal.deployment_id.clone(), session_id: Some(CHAT.into()), unknown: Default::default() };
    let workspace = LoroDoc::new();
    workspace.get_map("meta").insert("schemaVersion", 3i64).unwrap();
    workspace.get_map("chats").insert(CHAT, LoroValue::from(serde_json::json!({
        "id":CHAT, "deviceId":"host", "createdAt":1, "harnessSessionId":"native-unchanged",
        "harnessSessionCwd":"/repo", "cwd":"/repo", "archived":false,
        "config":{"harness":"omp", "sandbox":"read-only"},
    }))).unwrap();
    workspace.get_map("sessionRefs").insert(&format!("5:owner:{CHAT}"), LoroValue::from(serde_json::json!({
        "userId":"owner", "chatId":CHAT, "addedAt":1
    }))).unwrap();
    workspace.commit();
    let chat = LoroDoc::new();
    chat.get_map("meta").insert("chatId", CHAT).unwrap();
    chat.get_map("meta").insert("schemaVersion", 2i64).unwrap();
    for message in legacy_messages() { push_row(&chat, "messages", &message); }
    push_row(&chat, "commands", &legacy_command("command-1", if pending { "pending" } else { "applied" }));
    for (index, annotation) in [
        legacy_annotation("Original comment", "anchored", serde_json::Value::Null),
        legacy_annotation("Edited and resolved comment", "orphaned", 15.into()),
    ].into_iter().enumerate() {
        let id = format!("annotation/annotation-1/revision-{index}");
        push_row(&chat, "publications", &serde_json::json!({"id":id,"record":{
            "id":id,"schemaVersion":2,"publishedAt":13 + index,"publishedBy":"owner",
            "kind":"annotation","value":annotation
        }}));
    }
    push_row(&chat, "publications", &serde_json::json!({"id":"attachment-publication","record":{
        "id":"attachment-publication","schemaVersion":2,"publishedAt":10,"publishedBy":"owner",
        "kind":"attachment","value":{"id":"attachment-1","fileName":"retained.png","mimeType":"image/png",
            "sizeBytes":6,"blobKey":"retained-blob","createdBy":"owner","createdAt":10,
            "metadata":{"localPath":"/original/profile/uploads/retained.png"}}
    }}));
    chat.commit();
    let db = rusqlite::Connection::open(path.join("docs.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE snapshots(doc_id TEXT PRIMARY KEY,bytes BLOB); CREATE TABLE processed_commands(command_id TEXT PRIMARY KEY,processed_at INTEGER);").unwrap();
    db.execute("INSERT INTO snapshots VALUES (?1,?2)", rusqlite::params!["workspace3", workspace.export(ExportMode::Snapshot).unwrap()]).unwrap();
    db.execute("INSERT INTO snapshots VALUES (?1,?2)", rusqlite::params![CHAT, chat.export(ExportMode::Snapshot).unwrap()]).unwrap();
    drop(db);
    std::fs::create_dir(path.join("attachments")).unwrap();
    std::fs::write(path.join("attachments/image.png"), b"preserved bytes").unwrap();
    std::fs::write(path.join("settings.json"), b"{\"theme\":\"dark\"}").unwrap();
    std::fs::create_dir(path.join("uploads")).unwrap();
    std::fs::write(path.join("uploads/retained.png"), b"upload").unwrap();
    std::fs::create_dir(path.join("native-histories")).unwrap();
    std::fs::write(path.join("native-histories/omp.jsonl"), concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"native-unchanged\",\"cwd\":\"/repo\"}\n",
        "{\"type\":\"message\",\"id\":\"native-user-1\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"Inspect the image\"}}\n",
        "{\"type\":\"message\",\"id\":\"native-assistant-1\",\"parentId\":\"native-user-1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Retained native response\"},{\"type\":\"toolCall\",\"id\":\"exec-1\",\"name\":\"bash\",\"arguments\":{\"command\":\"printf 'fixture only'\"}}],\"stopReason\":\"toolUse\"}}\n",
        "{\"type\":\"message\",\"id\":\"native-result-1\",\"parentId\":\"native-assistant-1\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"exec-1\",\"toolName\":\"bash\",\"content\":[{\"type\":\"text\",\"text\":\"fixture only\"}],\"isError\":false}}\n",
    )).unwrap();
    let files = ["docs.sqlite3", "attachments/image.png", "uploads/retained.png", "settings.json", "native-histories/omp.jsonl"].into_iter().map(|name| {
        (name.into(), format!("{:x}", Sha256::digest(std::fs::read(path.join(name)).unwrap())))
    }).collect();
    let manifest = CrewBackupManifest { version:1, writer_stopped:true, owner_subject:principal.subject.clone(),
        source_profile_root:"/original/profile".into(),
        native_histories:BTreeMap::from([("native-unchanged".into(), "native-histories/omp.jsonl".into())]),
        attachment_aliases:BTreeMap::new(),
        desktop_source:None, original_epoch_histories:BTreeMap::new(),
        project_id:principal.project_id.clone(), deployment_id:principal.deployment_id.clone(), files,
        chats:BTreeMap::from([(CHAT.into(), CrewChatMetadata { owner_subject:principal.subject.clone(),
            scope, session_id:CHAT.into(), environment:None, agent_route:None, original_epoch_history:None })]) };
    std::fs::write(path.join("crew-backup.json"), serde_json::to_vec(&manifest).unwrap()).unwrap();
    principal
}

#[test]
fn backup_import_accepts_only_empty_known_startup_table() {
    for (table, populated, accepted) in [
        ("session_startups", false, true),
        ("session_startups", true, false),
        ("unknown_startups", false, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("backup");
        std::fs::create_dir(&source).unwrap();
        let principal = backup(&source, false);
        let db = rusqlite::Connection::open(source.join("docs.sqlite3")).unwrap();
        db.execute_batch(&format!("CREATE TABLE {table}(id TEXT PRIMARY KEY)")).unwrap();
        if populated {
            db.execute("INSERT INTO session_startups VALUES ('unresolved-startup')", []).unwrap();
        }
        drop(db);
        refresh_database_hash(&source);
        let target = root.path().join("new-profile");
        let source_bytes = std::fs::read(source.join("docs.sqlite3")).unwrap();
        let preflight = zeron_sync::crew_import::preflight_legacy_crew_backup(
            &source, &target, &principal, "controller").unwrap();
        assert_eq!(preflight["valid"], accepted);
        assert!(!target.exists());
        let result = import_legacy_crew_backup(&source, &target, &principal, "controller");
        if accepted {
            let report = result.unwrap();
            assert_eq!(report.chat_ids, [CHAT]);
            assert_eq!(report.command_outcomes["command-1"], "applied");
            assert_eq!(report.source_table_rows["session_startups"], 0);
            let verified = zeron_sync::crew_import::verify_legacy_crew_import(
                &source, &target, &principal, "controller").unwrap();
            assert_eq!(verified.source_table_rows, report.source_table_rows);
            assert_eq!(preflight["report"], serde_json::to_value(&report).unwrap());
            zeron_sync::crew_import::verify_imported_profile(
                &target, &principal, "controller", CHAT).unwrap();
            assert_eq!(std::fs::read(target.join("legacy-crew.sqlite3")).unwrap(), source_bytes);
            let store = DocsStore::open(&target).unwrap();
            assert!(store.is_processed("command-1").unwrap());
        } else {
            let expected = if populated { "session startups require legacy reconciliation" }
                else { "unknown legacy table" };
            assert!(result.unwrap_err().to_string().contains(expected));
            assert!(preflight["error"].as_str().unwrap().contains(expected));
            assert!(!target.exists());
        }
        assert_eq!(std::fs::read(source.join("docs.sqlite3")).unwrap(), source_bytes);
    }
}

#[test]
fn backup_import_preserves_ids_assets_and_terminal_outcomes_without_replay() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, false);
    let target = root.path().join("new-profile");
    let report = import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    for (relative, expected) in &report.files_sha256 {
        assert_eq!(format!("{:x}", Sha256::digest(std::fs::read(source.join(relative)).unwrap())), *expected,
            "conversion changed original source bytes: {relative}");
    }
    assert_eq!(report.native_session_ids[CHAT], "native-unchanged");
    assert_eq!(report.command_outcomes["command-1"], "applied");
    assert_eq!(std::fs::read(target.join("attachments/image.png")).unwrap(), b"preserved bytes");
    let aliases = zeron_sync::crew_import::imported_attachment_paths(&target).unwrap();
    assert_eq!(aliases, vec![
        (std::path::PathBuf::from("/original/profile/attachments/image.png"), target.join("attachments/image.png").canonicalize().unwrap()),
        (std::path::PathBuf::from("/original/profile/uploads/retained.png"), target.join("uploads/retained.png").canonicalize().unwrap()),
    ]);
    let histories = zeron_sync::crew_import::imported_native_histories(&target).unwrap();
    assert_eq!(histories["native-unchanged"], target.join("native-histories/omp.jsonl").canonicalize().unwrap());
    assert_eq!(std::fs::read(&histories["native-unchanged"]).unwrap(), std::fs::read(source.join("native-histories/omp.jsonl")).unwrap());
    assert_eq!(std::fs::read(target.join("legacy-crew.sqlite3")).unwrap(), std::fs::read(source.join("docs.sqlite3")).unwrap());
    let store = DocsStore::open(&target).unwrap();
    assert!(store.is_processed("command-1").unwrap());
    assert_eq!(store.legacy_crew_records(CHAT).unwrap().len(), 1);
    let registry = zeron_doc::RegistryDoc::from_bytes(&store.load_snapshot(zeron_doc::REGISTRY_DOC_ID).unwrap().unwrap(), "other-client").unwrap();
    assert_eq!(registry.chat(CHAT).unwrap().unwrap().crew.unwrap().owner_subject, "owner");
    assert_eq!(report.annotations_retained, 1, "revisions are not distinct annotations");
    let raw = LoroDoc::new();
    raw.import(&store.load_snapshot(CHAT).unwrap().unwrap()).unwrap();
    let doc = zeron_doc::SessionDoc::from_doc(raw);
    let entries = doc.read_entries().unwrap();
    assert_eq!(entries.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(), ["user-1", "assistant-1", "assistant-1#c1"]);
    assert_eq!(entries[2].continuation_of.as_deref(), Some("assistant-1"));
    for (entry, original) in entries.iter().zip(legacy_messages()) {
        assert_eq!(entry.device_id, original["deviceId"].as_str().unwrap());
        assert_eq!(entry.created_at, original["createdAt"].as_i64().unwrap());
        assert_eq!(entry.status, Some(zeron_doc::MessageStatus::Complete));
        for (part, original) in entry.parts.iter().zip(original["parts"].as_array().unwrap()) {
            assert_eq!(part.id(), original["id"].as_str().unwrap());
        }
        assert_eq!(entry.parts.len(), original["parts"].as_array().unwrap().len());
    }
    use zeron_doc::MessagePart;
    assert!(matches!(&entries[0].parts[0], MessagePart::Text { text, .. }
        if text == legacy_messages()[0]["parts"][0]["text"].as_str().unwrap()));
    assert!(matches!(&entries[1].parts[0], MessagePart::Text { text, .. } if text == "Résumé — image inspected."));
    assert!(matches!(&entries[1].parts[1], MessagePart::Tool {
        call: zeron_proto::ToolCall::Exec { command }, resolved: true, is_error: false, ..
    } if command == "printf 'fixture only'"));
    assert!(matches!(&entries[1].parts[2], MessagePart::Tool {
        call: zeron_proto::ToolCall::Todo { items }, resolved: true, ..
    } if items.len() == 1 && items[0].text == "Inspect image" && items[0].done));
    assert!(matches!(&entries[1].parts[3], MessagePart::Tool {
        call: zeron_proto::ToolCall::Unknown { input: Some(input), .. }, resolved: false, ..
    } if input == &legacy_messages()[1]["parts"][3]["call"]["input"]));
    assert!(matches!(&entries[1].parts[4], MessagePart::Input { request_id, questions, resolved: true, .. }
        if request_id == "request-1" && questions[0].question == "Keep history?" && questions[0].options == ["Yes"]));
    assert!(matches!(&entries[1].parts[5], MessagePart::Error { message, .. } if message == "Retained historical warning"));
    let MessagePart::Text { text, .. } = &entries[1].parts[6] else { panic!("missing lifecycle representation") };
    let flags: serde_json::Value = serde_json::from_str(text.strip_prefix("Legacy Crew thinking lifecycle: ").unwrap()).unwrap();
    assert_eq!(flags, legacy_messages()[1]["parts"][6]);
    assert!(matches!(&entries[1].parts[7], MessagePart::Tool {
        call: zeron_proto::ToolCall::Unknown { name, input: Some(input) }, resolved: true, is_error: true, ..
    } if name == "Legacy Crew Agent" && input == &legacy_messages()[1]["parts"][7]["call"]));
    assert_eq!(doc.read_opening_tail(1).unwrap()[0].parts.last(), entries[2].parts.last());
    assert_eq!(serde_json::to_value(doc.annotations().unwrap()).unwrap(),
        serde_json::json!([legacy_annotation("Edited and resolved comment", "orphaned", 15.into())]));
    let commands = doc.read_commands().unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].id, "command-1");
    assert_eq!(commands[0].status, zeron_doc::SessionCommandStatus::Applied);
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&store.legacy_crew_records(CHAT).unwrap()[0].2).unwrap(), legacy_command("command-1", "applied"));
    // Historical attachment publications and all annotation revisions remain
    // in the real target Loro document, not just in the import report.
    use loro::ToJson;
    let root = doc.doc().get_deep_value().to_json_value();
    assert_eq!(root["publications"][0]["record"]["value"], legacy_annotation("Original comment", "anchored", serde_json::Value::Null));
    let attachment: zeron_proto::PublicationRecord = serde_json::from_value(root["publications"][2]["record"].clone()).unwrap();
    assert!(matches!(attachment.value, zeron_proto::PublicationValue::Attachment(value)
        if value.id == "attachment-1" && value.blob_key == "retained-blob"
            && value.metadata["localPath"] == "/original/profile/uploads/retained.png"));
    let legacy = rusqlite::Connection::open_with_flags(target.join("legacy-crew.sqlite3"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let bytes: Vec<u8> = legacy.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [CHAT], |row| row.get(0)).unwrap();
    let original = LoroDoc::new();
    original.import(&bytes).unwrap();
    assert_eq!(original.get_list("messages").get_deep_value().to_json_value(), serde_json::json!(legacy_messages()));
    assert!(import_legacy_crew_backup(&source, &target, &principal, "controller").is_err());
    let archive = target.join("native-histories/omp.jsonl");
    let mut changed = std::fs::read(&archive).unwrap();
    changed.extend_from_slice(b"\n");
    std::fs::write(&archive, changed).unwrap();
    assert!(zeron_sync::crew_import::imported_native_histories(&target).unwrap_err().to_string().contains("integrity"));
}

#[test]
fn unknown_pending_work_and_tampered_assets_fail_before_destination_creation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, true);
    let target = root.path().join("new-profile");
    assert!(import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap_err().to_string().contains("unclaimed pending command"));
    assert!(!target.exists());
    std::fs::write(source.join("attachments/image.png"), b"changed").unwrap();
    assert!(import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap_err().to_string().contains("integrity mismatch"));
    assert!(!target.exists());
}

#[test]
fn immutable_claims_and_outcomes_survive_reopening() {
    let root = tempfile::tempdir().unwrap();
    let store = DocsStore::open(root.path()).unwrap();
    assert!(store.claim_crew_command("id", b"intent").unwrap());
    assert!(!store.claim_crew_command("id", b"intent").unwrap());
    assert!(store.claim_crew_command("id", b"changed").is_err());
    drop(store);
    let store = DocsStore::open(root.path()).unwrap();
    assert_eq!(store.crew_command("id").unwrap(), Some((b"intent".to_vec(), None)));
    store.finish_crew_command("id", b"confirmed").unwrap();
    store.finish_crew_command("id", b"confirmed").unwrap();
    assert!(store.finish_crew_command("id", b"rejected").is_err());
}

#[test]
fn durable_terminal_journal_outcome_reconciles_a_pending_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, true);
    let db = rusqlite::Connection::open(source.join("docs.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE semantic_intents(doc_id TEXT,container TEXT,record_id TEXT,value BLOB);
        CREATE TABLE document_scopes(doc_id TEXT PRIMARY KEY,scope TEXT NOT NULL);").unwrap();
    let cache_scope = serde_json::json!({"project":"project","principal":"owner","deployment":"canary","session":CHAT}).to_string();
    db.execute("INSERT INTO document_scopes VALUES (?1,?2)", rusqlite::params![CHAT, cache_scope]).unwrap();
    let record = serde_json::json!({
        "container":"commands", "key":"command-1", "before":null, "version":[], "acknowledged":false,
        "value":legacy_command("command-1", "applied")
    });
    db.execute("INSERT INTO semantic_intents VALUES (?1,'commands','command-1',?2)",
        rusqlite::params![cache_scope, serde_json::to_vec(&record).unwrap()]).unwrap();
    drop(db);
    refresh_database_hash(&source);
    let target = root.path().join("new-profile");
    let report = import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    assert_eq!(report.command_outcomes["command-1"], "applied");
    let store = DocsStore::open(target).unwrap();
    assert!(store.is_processed("command-1").unwrap());
    assert_eq!(store.legacy_crew_records(CHAT).unwrap().len(), 2);
    let raw = LoroDoc::new();
    raw.import(&store.load_snapshot(CHAT).unwrap().unwrap()).unwrap();
    let doc = zeron_doc::SessionDoc::from_doc(raw);
    assert_eq!(doc.read_commands().unwrap()[0].status, zeron_doc::SessionCommandStatus::Applied);
    let records = store.legacy_crew_records(CHAT).unwrap();
    let journal = records.iter().find(|(container, _, _)| container == "journal/commands").unwrap();
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&journal.2).unwrap(), record);
}

#[test]
fn non_omp_native_history_requires_isolated_restore_before_any_destination_write() {
    use loro::ToJson;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, false);
    let db = rusqlite::Connection::open(source.join("docs.sqlite3")).unwrap();
    let bytes: Vec<u8> = db.query_row("SELECT bytes FROM snapshots WHERE doc_id='workspace3'", [], |row| row.get(0)).unwrap();
    let workspace = LoroDoc::new();
    workspace.import(&bytes).unwrap();
    let mut row = workspace.get_map("chats").get(CHAT).unwrap().get_deep_value().to_json_value();
    row["config"]["harness"] = "codex".into();
    workspace.get_map("chats").insert(CHAT, LoroValue::from(row)).unwrap();
    workspace.commit();
    db.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id='workspace3'", [workspace.export(ExportMode::Snapshot).unwrap()]).unwrap();
    drop(db);
    let manifest_path = source.join("crew-backup.json");
    let mut manifest: CrewBackupManifest = serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest.files.insert("docs.sqlite3".into(), format!("{:x}", Sha256::digest(std::fs::read(source.join("docs.sqlite3")).unwrap())));
    std::fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let target = root.path().join("new-profile");
    let error = import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap_err().to_string();
    assert!(error.contains("external native-store restore prerequisite"));
    assert!(error.contains("native-unchanged"));
    assert!(error.contains("/repo"));
    assert!(!target.exists());
}

#[test]
fn claimed_unknown_and_every_terminal_outcome_remain_non_replayable() {
    use zeron_doc::commands::{CommandDisposition, EvaluationContext, evaluate_command};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, true);
    let db = rusqlite::Connection::open(source.join("docs.sqlite3")).unwrap();
    db.execute("INSERT INTO processed_commands VALUES ('command-1',42),('claim-without-outcome',43)", []).unwrap();
    let bytes: Vec<u8> = db.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [CHAT], |row| row.get(0)).unwrap();
    let raw = LoroDoc::new();
    raw.import(&bytes).unwrap();
    for status in ["applied", "rejected", "expired", "superseded", "cancelled"] {
        push_row(&raw, "commands", &legacy_command(status, status));
    }
    let mut control = legacy_command("legacy-control", "applied");
    control["kind"] = "control".into();
    control["payload"] = serde_json::json!({"kind":"control","sessionId":CHAT,
        "ownerDeviceId":"host","actorDeviceId":"host","actorSubject":"owner","grantId":"historical-grant",
        "source":"local","action":{"action":"annotationResolve","annotation_id":"annotation-1","resolved":true}});
    push_row(&raw, "commands", &control);
    raw.commit();
    db.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id=?2", rusqlite::params![raw.export(ExportMode::Snapshot).unwrap(), CHAT]).unwrap();
    drop(db);
    refresh_database_hash(&source);
    let target = root.path().join("new-profile");
    let report = import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    assert_eq!(report.command_outcomes["command-1"], "unknown");
    assert_eq!(report.command_outcomes["claim-without-outcome"], "unknown");
    assert_eq!(report.command_outcomes["legacy-control"], "applied");
    for status in ["applied", "rejected", "expired", "superseded", "cancelled"] {
        assert_eq!(report.command_outcomes[status], status);
    }
    let store = DocsStore::open(&target).unwrap();
    let raw = LoroDoc::new();
    raw.import(&store.load_snapshot(CHAT).unwrap().unwrap()).unwrap();
    let doc = zeron_doc::SessionDoc::from_doc(raw);
    let commands = doc.read_commands().unwrap();
    assert_eq!(commands.len(), 6);
    assert_eq!(commands[0].status, zeron_doc::SessionCommandStatus::Pending);
    let processed = |id: &str| store.is_processed(id).unwrap();
    let context = EvaluationContext { is_processed: &processed, now_ms: 0, entries: &commands,
        current_turn_id: Some("assistant-1"), turn_is_past: &|_| false };
    for command in &commands {
        assert_eq!(evaluate_command(command, &context), CommandDisposition::Skip);
    }
    assert!(store.is_processed("claim-without-outcome").unwrap());
    assert!(store.is_processed("legacy-control").unwrap());
    let records = store.legacy_crew_records(CHAT).unwrap();
    let retained = records.iter().find(|(container, id, _)| container == "commands" && id == "legacy-control").unwrap();
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&retained.2).unwrap(), control);
    drop(store);
    let store = DocsStore::open(&target).unwrap();
    assert!(store.is_processed("command-1").unwrap());
    assert!(store.is_processed("claim-without-outcome").unwrap());
}

#[test]
fn peer_provenance_appends_without_rewriting_original_parts_and_delivery_is_not_invented() {
    for status in ["complete", "queued", "steered"] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("backup");
        std::fs::create_dir(&source).unwrap();
        let principal = backup(&source, false);
        let db = rusqlite::Connection::open(source.join("docs.sqlite3")).unwrap();
        let bytes: Vec<u8> = db.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [CHAT], |row| row.get(0)).unwrap();
        let raw = LoroDoc::new();
        raw.import(&bytes).unwrap();
        let mut peer = legacy_messages().remove(0);
        peer["id"] = "peer-command".into();
        peer["status"] = status.into();
        peer["peerMessage"] = serde_json::json!({"commandId":"peer-command","sourceChatId":"peer-source",
            "threadId":"thread-stable","replyTo":"prior-command"});
        push_row(&raw, "messages", &peer);
        raw.commit();
        db.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id=?2", rusqlite::params![raw.export(ExportMode::Snapshot).unwrap(), CHAT]).unwrap();
        drop(db);
        refresh_database_hash(&source);
        let target = root.path().join("new-profile");
        let result = import_legacy_crew_backup(&source, &target, &principal, "controller");
        if status != "complete" {
            assert!(result.unwrap_err().to_string().contains("queued/steered delivery requires reconciliation"));
            assert!(!target.exists());
            continue;
        }
        result.unwrap();
        let store = DocsStore::open(target).unwrap();
        let raw = LoroDoc::new();
        raw.import(&store.load_snapshot(CHAT).unwrap().unwrap()).unwrap();
        let doc = zeron_doc::SessionDoc::from_doc(raw);
        let entries = doc.read_entries().unwrap();
        let imported = entries.last().unwrap();
        assert_eq!(imported.id, "peer-command");
        assert_eq!(imported.parts[0], entries[0].parts[0]);
        assert_eq!(imported.parts.len(), 2);
        let zeron_doc::MessagePart::Text { id, text } = &imported.parts[1] else { panic!("missing provenance") };
        assert_eq!(id, "peer-command/legacy-peer-provenance");
        let provenance: serde_json::Value = serde_json::from_str(text.strip_prefix(
            "Legacy Crew peer provenance (historical metadata, not authorization): ").unwrap()).unwrap();
        assert_eq!(provenance, peer["peerMessage"]);
        let records = store.legacy_crew_records(CHAT).unwrap();
        let record = records.iter().find(|(container, id, _)| container == "messages" && id == "peer-command").unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&record.2).unwrap(), peer);
    }
}

#[test]
fn shared_agent_membership_and_granted_environment_are_explicit_blockers() {
    for container in ["publications", "agentSessions", "sessionRefs"] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("backup");
        std::fs::create_dir(&source).unwrap();
        let principal = backup(&source, false);
        let db = rusqlite::Connection::open(source.join("docs.sqlite3")).unwrap();
        let doc_id = if container == "sessionRefs" { "workspace3" } else { CHAT };
        let bytes: Vec<u8> = db.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [doc_id], |row| row.get(0)).unwrap();
        let raw = LoroDoc::new();
        raw.import(&bytes).unwrap();
        let record = serde_json::json!({"id":"foreign-agent","schemaVersion":2,"publishedAt":1,"publishedBy":"foreign",
            "kind":"agentSession","value":{"sessionId":CHAT,"chatId":CHAT,"ownerSubject":"foreign",
                "ownerDeviceId":"foreign-host","source":"local","createdAt":1}});
        match container {
            "publications" => push_row(&raw, container, &serde_json::json!({"id":"foreign-agent","record":record})),
            "agentSessions" => { raw.get_map(container).insert(CHAT, LoroValue::from(record)).unwrap(); }
            _ => {
                raw.get_map(container).insert(&format!("5:owner:{CHAT}"), LoroValue::from(serde_json::json!({
                    "userId":"owner","chatId":CHAT,"addedAt":1,"environment":{
                        "source":{"kind":"local"},"ownerPrincipal":"foreign",
                        "scope":{"projectId":"project","deploymentId":"canary","sessionId":CHAT}
                    }
                }))).unwrap();
            }
        }
        raw.commit();
        db.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id=?2", rusqlite::params![raw.export(ExportMode::Snapshot).unwrap(), doc_id]).unwrap();
        drop(db);
        refresh_database_hash(&source);
        let target = root.path().join("new-profile");
        let error = import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap_err().to_string();
        assert!(error.contains("membership requires authenticated reattachment"), "{error}");
        assert!(!target.exists());
    }
}

#[test]
fn legacy_0162_preflight_matches_import_and_keeps_recovery_evidence_out_of_rooms() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, false);
    let db = rusqlite::Connection::open(source.join("docs.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE session_startups(chat_id TEXT PRIMARY KEY,worktree_path TEXT,cancelled INTEGER NOT NULL DEFAULT 0);").unwrap();
    // This original cache is evidence, not a second chat and not necessarily a
    // valid current Loro snapshot (the original legacy store retains zeroblobs).
    db.execute("INSERT INTO snapshots VALUES ('recovery-original/prior-scope',?1)", [b"original evidence".as_slice()]).unwrap();
    drop(db);
    refresh_database_hash(&source);
    let target = root.path().join("new-profile");
    let before = std::fs::read(source.join("docs.sqlite3")).unwrap();
    let report = zeron_sync::crew_import::validate_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    assert!(!target.exists());
    assert_eq!(std::fs::read(source.join("docs.sqlite3")).unwrap(), before);
    assert_eq!(report.source_table_rows["snapshots"], 3);
    assert_eq!(report.source_table_rows["session_startups"], 0);
    assert_eq!(report.recovery_snapshot_ids, ["recovery-original/prior-scope"]);
    let imported = import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    assert_eq!(serde_json::to_value(report).unwrap(), serde_json::to_value(imported).unwrap());
    let store = DocsStore::open(&target).unwrap();
    assert!(store.load_snapshot("recovery-original/prior-scope").unwrap().is_none());
    let evidence = store.legacy_crew_records("recovery-original/prior-scope").unwrap();
    assert_eq!(evidence[0].2, b"original evidence");
}

#[test]
fn interrupted_startups_and_existing_canary_profiles_are_not_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, false);
    let db = rusqlite::Connection::open(source.join("docs.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE session_startups(chat_id TEXT PRIMARY KEY,worktree_path TEXT,cancelled INTEGER NOT NULL DEFAULT 0);
        INSERT INTO session_startups VALUES ('pending','/worktree',1);").unwrap();
    drop(db);
    refresh_database_hash(&source);
    let target = root.path().join("new-profile");
    let error = zeron_sync::crew_import::validate_legacy_crew_backup(&source, &target, &principal, "controller").unwrap_err();
    assert!(error.to_string().contains("session startups require legacy reconciliation"));
    assert!(!target.exists());
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("canary-authority"), b"unchanged").unwrap();
    assert!(import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap_err().to_string().contains("new separate profile"));
    assert_eq!(std::fs::read(target.join("canary-authority")).unwrap(), b"unchanged");
}

#[test]
fn runtime_rejects_legacy_schema_and_incomplete_import_before_mutating_database() {
    let root = tempfile::tempdir().unwrap();
    backup(root.path(), false);
    let original = std::fs::read(root.path().join("docs.sqlite3")).unwrap();
    let error = match DocsStore::open(root.path()) { Ok(_) => panic!("legacy profile opened"), Err(error) => error };
    assert!(error.to_string().contains("legacy Crew database"));
    assert_eq!(std::fs::read(root.path().join("docs.sqlite3")).unwrap(), original);
    let target = root.path().join("incomplete");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("crew-import-incomplete"), b"in progress").unwrap();
    assert!(DocsStore::open(&target).is_err());
    assert!(!target.join("docs.sqlite3").exists());
}

#[test]
fn blocked_preflight_reports_all_records_without_creating_a_profile() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, true);
    let target = root.path().join("new-profile");
    let report = zeron_sync::crew_import::preflight_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    assert_eq!(report["valid"], false);
    assert_eq!(report["sourceTableRows"]["snapshots"], 2);
    assert_eq!(report["snapshots"][CHAT], "public-session");
    assert!(report["error"].as_str().unwrap().contains("unclaimed pending command"));
    assert!(!target.exists());
}

#[test]
fn local_history_retains_native_identity_without_cloud_metadata() {
    use loro::ToJson;
    const LOCAL: &str = "local-chat-0123456789abcdef";
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, false);
    let db = rusqlite::Connection::open(source.join("docs.sqlite3")).unwrap();
    let bytes: Vec<u8> = db.query_row("SELECT bytes FROM snapshots WHERE doc_id='workspace3'", [], |row| row.get(0)).unwrap();
    let workspace = LoroDoc::new();
    workspace.import(&bytes).unwrap();
    let mut row = workspace.get_map("chats").get(CHAT).unwrap().get_deep_value().to_json_value();
    row["id"] = LOCAL.into();
    workspace.get_map("chats").delete(CHAT).unwrap();
    workspace.get_map("chats").insert(LOCAL, LoroValue::from(row)).unwrap();
    workspace.get_map("sessionRefs").delete(&format!("5:owner:{CHAT}")).unwrap();
    workspace.commit();
    db.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id='workspace3'", [workspace.export(ExportMode::Snapshot).unwrap()]).unwrap();
    let bytes: Vec<u8> = db.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [CHAT], |row| row.get(0)).unwrap();
    let chat = LoroDoc::new();
    chat.import(&bytes).unwrap();
    chat.get_map("meta").insert("chatId", LOCAL).unwrap();
    chat.commit();
    db.execute("UPDATE snapshots SET doc_id=?1,bytes=?2 WHERE doc_id=?3", rusqlite::params![LOCAL, chat.export(ExportMode::Snapshot).unwrap(), CHAT]).unwrap();
    drop(db);
    let path = source.join("crew-backup.json");
    let mut manifest: CrewBackupManifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut metadata = manifest.chats.remove(CHAT).unwrap();
    metadata.session_id = LOCAL.into();
    metadata.scope.session_id = Some(LOCAL.into());
    manifest.chats.insert(LOCAL.into(), metadata);
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    refresh_database_hash(&source);
    let target = root.path().join("new-profile");
    let report = import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    assert_eq!(report.chat_ids, [LOCAL]);
    assert_eq!(report.native_session_ids[LOCAL], "native-unchanged");
    let store = DocsStore::open(&target).unwrap();
    let registry = zeron_doc::RegistryDoc::from_bytes(&store.load_snapshot(zeron_doc::REGISTRY_DOC_ID).unwrap().unwrap(), "controller").unwrap();
    let row = registry.chat(LOCAL).unwrap().unwrap();
    assert!(row.crew.is_none());
    assert_eq!(row.harness_session_id.as_deref(), Some("native-unchanged"));
    assert!(store.load_snapshot(LOCAL).unwrap().is_some());
}

#[test]
fn unassociated_bootstrap_is_retained_as_evidence_not_a_conversation() {
    const ORPHAN: &str = "unassociated-bootstrap";
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, false);
    let orphan = LoroDoc::new();
    orphan.get_map("meta").insert("chatId", ORPHAN).unwrap();
    orphan.get_map("meta").insert("schemaVersion", 2i64).unwrap();
    orphan.get_list("messages");
    orphan.get_list("commands");
    orphan.commit();
    let bytes = orphan.export(ExportMode::Snapshot).unwrap();
    let database = rusqlite::Connection::open(source.join("docs.sqlite3")).unwrap();
    database.execute("INSERT INTO snapshots(doc_id,bytes) VALUES (?1,?2)", rusqlite::params![ORPHAN, &bytes]).unwrap();
    drop(database);
    refresh_database_hash(&source);
    let target = root.path().join("native");
    let report = import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    let disposition = &report.inert_bootstrap_snapshots[ORPHAN];
    assert_eq!(disposition.disposition, "inert-bootstrap");
    assert_eq!(disposition.snapshot_sha256, format!("{:x}", Sha256::digest(&bytes)));
    assert!(!report.chat_ids.iter().any(|id| id == ORPHAN));
    let store = DocsStore::open(&target).unwrap();
    assert!(store.load_snapshot(ORPHAN).unwrap().is_none());
    assert_eq!(store.legacy_crew_records(ORPHAN).unwrap(), vec![("inert-bootstrap".into(), ORPHAN.into(), bytes.clone())]);
    let registry = zeron_doc::RegistryDoc::from_bytes(&store.load_snapshot(zeron_doc::REGISTRY_DOC_ID).unwrap().unwrap(), "controller").unwrap();
    assert!(registry.chat(ORPHAN).unwrap().is_none());
    let retained = rusqlite::Connection::open_with_flags(target.join("legacy-crew.sqlite3"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(retained.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [ORPHAN], |row| row.get::<_, Vec<u8>>(0)).unwrap(), bytes);
    let verified = zeron_sync::crew_import::verify_legacy_crew_import(&source, &target, &principal, "controller").unwrap();
    assert_eq!(verified.inert_bootstrap_snapshots, report.inert_bootstrap_snapshots);
}

#[test]
fn destination_readback_uses_database_not_claimed_report() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, false);
    let target = root.path().join("new-profile");
    import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    // Neither a forged operational report nor unchanged claimed receipts can
    // substitute for the source-derived actual database/asset comparison.
    let original_report = std::fs::read(target.join("crew-import-report.json")).unwrap();
    std::fs::write(target.join("crew-import-report.json"), b"{}").unwrap();
    assert!(zeron_sync::crew_import::verify_legacy_crew_import(&source, &target, &principal, "controller").is_err());
    std::fs::write(target.join("crew-import-report.json"), original_report).unwrap();
    let report = zeron_sync::crew_import::verify_legacy_crew_import(&source, &target, &principal, "controller").unwrap();
    assert_eq!(report.destination_snapshots_sha256.len(), 2);
    assert_eq!(report.command_outcomes["command-1"], "applied");
    let db = rusqlite::Connection::open(target.join("docs.sqlite3")).unwrap();
    db.execute("DELETE FROM processed_commands WHERE command_id='command-1'", []).unwrap();
    drop(db);
    let error = zeron_sync::crew_import::verify_legacy_crew_import(&source, &target, &principal, "controller").unwrap_err();
    assert!(error.to_string().contains("durable command claims differ"));
}

#[test]
fn selected_profile_proof_uses_actual_history_and_allows_only_read_marker_progress() {
    use zeron_sync::crew_import::verify_imported_profile;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, false);
    let target = root.path().join("new-profile");
    import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    let store = DocsStore::open(&target).unwrap();
    assert_eq!(store.profile_root().unwrap(), target.canonicalize().unwrap());
    let mut registry = zeron_doc::RegistryDoc::from_bytes(
        &store.load_snapshot(zeron_doc::REGISTRY_DOC_ID).unwrap().unwrap(), "controller").unwrap();
    registry.set_chat_seen(CHAT, chrono::DateTime::from_timestamp_millis(100).unwrap()).unwrap();
    store.save_snapshot(zeron_doc::REGISTRY_DOC_ID, &registry.to_bytes().unwrap()).unwrap();
    store.save_snapshot("22222222-2222-4222-8222-222222222222", b"unrelated live snapshot").unwrap();
    store.mark_processed("unrelated-live-command").unwrap();
    let proof = verify_imported_profile(&target, &principal, "controller", CHAT).unwrap();
    assert_eq!(proof.selected_chat_id.as_deref(), Some(CHAT));
    assert_eq!(proof.chat_ids, [CHAT]);
    assert_eq!(proof.command_outcomes, BTreeMap::from([("command-1".into(), "applied".into())]));
    assert_eq!(proof.annotations_retained, 1);
    assert_eq!(proof.destination_snapshots_sha256.len(), 2);
    assert!(verify_imported_profile(&target, &principal, "controller", "22222222-2222-4222-8222-222222222222").is_err());
    let mut foreign = principal.clone(); foreign.subject = "other-owner".into();
    assert!(verify_imported_profile(&target, &foreign, "controller", CHAT).is_err());
    let mut chat = registry.chat(CHAT).unwrap().unwrap();
    chat.cwd = Some("/unrelated-repository".into());
    registry.upsert_chat(&chat).unwrap();
    store.save_snapshot(zeron_doc::REGISTRY_DOC_ID, &registry.to_bytes().unwrap()).unwrap();
    assert!(verify_imported_profile(&target, &principal, "controller", CHAT).unwrap_err().to_string().contains("registry identity/config/native"));
}

#[test]
fn explicit_legacy_host_provider_binding_survives_import() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, false);
    let db = rusqlite::Connection::open(source.join("docs.sqlite3")).unwrap();
    let bytes: Vec<u8> = db.query_row("SELECT bytes FROM snapshots WHERE doc_id='workspace3'", [], |row| row.get(0)).unwrap();
    let workspace = LoroDoc::new();
    workspace.import(&bytes).unwrap();
    workspace.get_map("devices").insert("host", LoroValue::from(serde_json::json!({
        "id":"host","name":"Preserved host","platform":"linux","environment":"namespace","namespaceDevboxId":"instance-id"
    }))).unwrap();
    workspace.commit();
    db.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id='workspace3'", [workspace.export(ExportMode::Snapshot).unwrap()]).unwrap();
    drop(db);
    refresh_database_hash(&source);
    let target = root.path().join("new-profile");
    import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    let store = DocsStore::open(&target).unwrap();
    let registry = zeron_doc::RegistryDoc::from_bytes(&store.load_snapshot(zeron_doc::REGISTRY_DOC_ID).unwrap().unwrap(), "controller").unwrap();
    let devices = registry.read_devices().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].devbox_id.as_deref(), Some("instance-id"));
    assert_eq!(devices[0].devbox_provider, Some(zeron_proto::DeviceEnvironment::Namespace));
}

#[test]
fn legacy_recovery_requests_are_retained_but_never_replayed_or_silently_retired() {
    for pending in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("backup");
        std::fs::create_dir(&source).unwrap();
        let principal = backup(&source, false);
        std::fs::create_dir(source.join("legacy-journals")).unwrap();
        let recovery = format!("legacy-journals/{CHAT}.recovery");
        let events = format!("legacy-journals/{CHAT}.jsonl");
        let request = if pending { serde_json::json!({"prompt":"accepted but not reconciled","resume":"native-unchanged"}) } else { serde_json::Value::Null };
        std::fs::write(source.join(&recovery), serde_json::to_vec(&serde_json::json!([CHAT, request])).unwrap()).unwrap();
        std::fs::write(source.join(&events), b"{\"seq\":0,\"event\":{\"type\":\"done\",\"status\":\"completed\"}}\n").unwrap();
        let path = source.join("crew-backup.json");
        let mut manifest: CrewBackupManifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for relative in [&recovery, &events] {
            manifest.files.insert(relative.clone(), format!("{:x}", Sha256::digest(std::fs::read(source.join(relative)).unwrap())));
        }
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let target = root.path().join("new-profile");
        if pending {
            let report = zeron_sync::crew_import::preflight_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
            assert_eq!(report["valid"], false);
            assert_eq!(report["legacyJournals"]["states"][&recovery], "pending-recovery");
            assert!(import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap_err().to_string().contains("execution journal requires reconciliation"));
            assert!(!target.exists());
        } else {
            let report = import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
            assert_eq!(report.legacy_journal_states[&recovery], "retired");
            assert_eq!(std::fs::read(target.join(&recovery)).unwrap(), std::fs::read(source.join(&recovery)).unwrap());
            assert!(!target.join("journals").exists());
        }
    }
}

#[test]
fn known_interrupted_pending_run_keeps_exact_request_account_and_native_identity() {
    for case in ["interrupted", "completed", "errored", "unknown", "no-stop", "wrong-context"] {
        let status = if matches!(case, "no-stop" | "wrong-context") { "interrupted" } else { case };
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("backup");
        std::fs::create_dir(&source).unwrap();
        let principal = backup(&source, false);
        std::fs::create_dir(source.join("legacy-journals")).unwrap();
        let identity = serde_json::json!({"SignedIn":{"owner_subject":"owner","project_scope":"project"}});
        let context_request = serde_json::json!({"prompt":"exact accepted request","model":null,"reasoning":null,
            "agentAccountId":"opaque-account","modelOptions":{"retained":"choice"},"cwd":"/repo",
            "sandbox":"read-only","autoApprove":false,"resume":null,"attachments":["/original/profile/uploads/retained.png"]});
        let mut pending_request = context_request.clone();
        pending_request["resume"] = "native-unchanged".into();
        let pending = serde_json::json!({"identity":identity,"device_id":"host","harness":"omp","request":pending_request,
            "user_message_id":"user-1","blocked_session":null,"updated_at":12,"journal_seq":1});
        let recovery = format!("legacy-journals/{CHAT}.recovery");
        let context = format!("legacy-journals/{CHAT}.context");
        let events = format!("legacy-journals/{CHAT}.jsonl");
        std::fs::write(source.join(&recovery), serde_json::to_vec(&serde_json::json!([CHAT,pending])).unwrap()).unwrap();
        let context_device = if case == "wrong-context" { "foreign-host" } else { "host" };
        std::fs::write(source.join(&context), serde_json::to_vec(&serde_json::json!([CHAT,[identity,context_device,"omp",context_request]])).unwrap()).unwrap();
        std::fs::write(source.join(&events), serde_json::to_vec(&serde_json::json!({"seq":2,"event":{
            "type":"done","status":status,"sessionId":"native-unchanged"}})).unwrap()).unwrap();
        let path = source.join("crew-backup.json");
        let mut manifest: CrewBackupManifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if case == "no-stop" { manifest.writer_stopped = false; }
        for relative in [&recovery, &context, &events] {
            manifest.files.insert(relative.clone(), format!("{:x}", Sha256::digest(std::fs::read(source.join(relative)).unwrap())));
        }
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let target = root.path().join("new-profile");
        let result = import_legacy_crew_backup(&source, &target, &principal, "controller");
        if case != "interrupted" {
            assert!(result.is_err(), "{case} must not become resumable work");
            assert!(!target.exists());
            continue;
        }
        let report = result.unwrap();
        assert_eq!(report.legacy_journal_states[&recovery], "validated-interrupted-request");
        let intents = zeron_sync::crew_import::imported_recovery_intents(&target).unwrap();
        assert_eq!(intents.len(), 1);
        let intent = &intents[0];
        assert_eq!(report.recovery_intent_ids[CHAT], intent.command_id);
        assert_eq!(intent.request.prompt, "exact accepted request");
        assert_eq!(intent.request.cwd, "/repo");
        assert_eq!(intent.request.resume.as_deref(), Some("native-unchanged"));
        assert_eq!(intent.agent_account_id.as_deref(), Some("opaque-account"));
        assert_eq!(intent.request.model_options["crewAccountId"], "opaque-account");
        assert!(!intent.request.auto_approve);
        assert_eq!(intent.destination_device_id, "controller");
        assert_eq!(intent.source_device_id, "host");
        assert_eq!(intent.user_message_id, "user-1");
        zeron_sync::crew_import::verify_legacy_crew_import(&source, &target, &principal, "controller").unwrap();
        std::fs::write(target.join(&context), b"changed").unwrap();
        assert!(zeron_sync::crew_import::imported_recovery_intents(&target).is_err());
    }
}

#[test]
fn explicit_attachment_references_resolve_only_to_inventoried_copies() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("backup");
    std::fs::create_dir(&source).unwrap();
    let principal = backup(&source, false);
    let path = source.join("crew-backup.json");
    let mut manifest: CrewBackupManifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest.attachment_aliases.insert("/attachments/remote-blob".into(), "attachments/image.png".into());
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let target = root.path().join("new-profile");
    import_legacy_crew_backup(&source, &target, &principal, "controller").unwrap();
    let aliases = zeron_sync::crew_import::imported_attachment_paths(&target).unwrap();
    assert!(aliases.contains(&(std::path::PathBuf::from("/attachments/remote-blob"), target.join("attachments/image.png").canonicalize().unwrap())));
    zeron_sync::crew_import::verify_legacy_crew_import(&source, &target, &principal, "controller").unwrap();
    std::fs::write(target.join("attachments/image.png"), b"tampered remote bytes").unwrap();
    assert!(zeron_sync::crew_import::imported_attachment_paths(&target).is_err());
    manifest.attachment_aliases.insert("/original/profile/uploads/retained.png".into(), "attachments/image.png".into());
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let rejected = root.path().join("conflict");
    assert!(import_legacy_crew_backup(&source, &rejected, &principal, "controller").unwrap_err().to_string().contains("conflicts with an inventoried original path"));
    assert!(!rejected.exists());
}
