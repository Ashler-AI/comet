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
        project_id:principal.project_id.clone(), deployment_id:principal.deployment_id.clone(), files,
        chats:BTreeMap::from([(CHAT.into(), CrewChatMetadata { owner_subject:principal.subject.clone(),
            scope, session_id:CHAT.into(), environment:None, agent_route:None })]) };
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
        let result = import_legacy_crew_backup(&source, &target, &principal, "controller");
        if accepted {
            let report = result.unwrap();
            assert_eq!(report.chat_ids, [CHAT]);
            assert_eq!(report.command_outcomes["command-1"], "applied");
            let store = DocsStore::open(&target).unwrap();
            assert!(store.is_processed("command-1").unwrap());
        } else {
            assert!(result.is_err(), "import accepted {table}, populated={populated}");
            assert!(!target.exists());
        }
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
    assert_eq!(report.native_session_ids[CHAT], "native-unchanged");
    assert_eq!(report.command_outcomes["command-1"], "applied");
    assert_eq!(std::fs::read(target.join("attachments/image.png")).unwrap(), b"preserved bytes");
    let aliases = zeron_sync::crew_import::imported_attachment_paths(&target).unwrap();
    assert_eq!(aliases, vec![(std::path::PathBuf::from("/original/profile/uploads/retained.png"), target.join("uploads/retained.png").canonicalize().unwrap())]);
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
