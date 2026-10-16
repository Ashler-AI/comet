//! Explicit, offline Crew backup import. Never open a live legacy profile with
//! the upstream runtime. A hash inventory and verified identity bind the input;
//! publication uses the ordinary personal registry, not the old workspace room.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use loro::{LoroDoc, LoroValue, ToJson};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeron_doc::{RegistryDoc, SessionDoc, REGISTRY_DOC_ID};
use zeron_proto::{CollaborationPrincipal, CrewChatMetadata, PublicationRecord, PublicationValue};
use crate::{DocsStore, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum CrewImportError {
    #[error("unsupported or unsafe Crew backup: {0}")]
    Unsafe(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Loro(#[from] loro::LoroError),
    #[error(transparent)]
    Doc(#[from] zeron_doc::DocError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// `crew-backup.json` next to the checkpointed `docs.sqlite3`. Every file in
/// the backup must be inventoried; assets keep their profile-relative paths.
/// Credentials must be excluded and reauthenticated in the destination.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CrewBackupManifest {
    pub version: u32,
    pub writer_stopped: bool,
    /// Original absolute profile path used by stored attachment references.
    pub source_profile_root: String,
    pub owner_subject: String,
    pub project_id: String,
    pub deployment_id: Option<String>,
    pub files: BTreeMap<String, String>,
    pub chats: BTreeMap<String, CrewChatMetadata>,
    /// OMP native ID -> inventoried `native-histories/…` journal copy.
    #[serde(default)]
    pub native_histories: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrewImportReport {
    pub version: u32,
    pub source_profile_root: String,
    pub chat_ids: Vec<String>,
    pub native_session_ids: BTreeMap<String, String>,
    #[serde(default)]
    pub native_histories: BTreeMap<String, String>,
    pub files_sha256: BTreeMap<String, String>,
    pub commands_retained: usize,
    /// Claimed without a durable terminal result stays unknown, never rejected
    /// or eligible for replay merely because import crossed a profile boundary.
    #[serde(default)]
    pub command_outcomes: BTreeMap<String, String>,
    pub annotations_retained: usize,
    pub requires_reauthentication: bool,
}

/// Load once when constructing the profile's upload resolver. Exact legacy
/// paths resolve to inventoried copies, never to the original live directory.
pub fn imported_attachment_paths(profile: &Path) -> Result<Vec<(std::path::PathBuf, std::path::PathBuf)>, CrewImportError> {
    let report_path = profile.join("crew-import-report.json");
    let bytes = match std::fs::read(report_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let report: CrewImportReport = serde_json::from_slice(&bytes)?;
    if report.version != 1 || !Path::new(&report.source_profile_root).is_absolute() {
        return Err(unsafe_data("invalid Crew import attachment report"));
    }
    let mut paths = Vec::new();
    let uploads = profile.join("uploads").canonicalize().ok();
    for (relative, expected) in report.files_sha256.iter().filter(|(path, _)| path.starts_with("uploads/")) {
        let relative = checked_path(relative)?;
        let destination = profile.join(relative).canonicalize()?;
        if !uploads.as_ref().is_some_and(|root| destination.starts_with(root) && &destination != root)
            || hash_file(&destination)? != *expected {
            return Err(unsafe_data("imported upload is outside its jail or failed integrity"));
        }
        paths.push((Path::new(&report.source_profile_root).join(relative), destination));
    }
    Ok(paths)
}

fn native_history_id(path: &Path) -> Result<String, CrewImportError> {
    use std::io::{BufRead, Read};
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file.take(64 * 1024));
    for line in reader.lines() {
        let line = line?;
        if let Ok(value) = serde_json::from_str::<Value>(&line)
            && value.get("type").and_then(Value::as_str) == Some("session") {
            return value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())
                .map(str::to_owned).ok_or_else(|| unsafe_data("OMP history header has no native ID"));
        }
    }
    Err(unsafe_data("OMP history has no bounded session header"))
}

/// Load once into host launch state. Journals are allowed to append after
/// import; their initial bytes are hash-verified during import, while every
/// subsequent launch remains bound to the preserved header ID and jailed path.
pub fn imported_native_histories(profile: &Path) -> Result<BTreeMap<String, std::path::PathBuf>, CrewImportError> {
    let bytes = match std::fs::read(profile.join("crew-import-report.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let report: CrewImportReport = serde_json::from_slice(&bytes)?;
    if report.version != 1 { return Err(unsafe_data("unknown native-history import report")); }
    let mut histories = BTreeMap::new();
    let root = profile.join("native-histories").canonicalize().ok();
    for (id, relative) in report.native_histories {
        let path = profile.join(checked_path(&relative)?).canonicalize()?;
        if !root.as_ref().is_some_and(|root| path.starts_with(root) && &path != root)
            || native_history_id(&path)? != id {
            return Err(unsafe_data("native history identity or destination jail mismatch"));
        }
        histories.insert(id, path);
    }
    Ok(histories)
}

fn unsafe_data(message: impl Into<String>) -> CrewImportError {
    CrewImportError::Unsafe(message.into())
}

fn inventory(root: &Path, dir: &Path, files: &mut BTreeSet<String>) -> Result<(), CrewImportError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() || (!kind.is_file() && !kind.is_dir()) {
            return Err(unsafe_data("backup contains a symlink or special file"));
        }
        if kind.is_dir() { inventory(root, &entry.path(), files)?; }
        else {
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|_| unsafe_data("invalid backup path"))?;
            files.insert(relative.to_str().ok_or_else(|| unsafe_data("non-UTF8 backup path"))?.to_owned());
        }
    }
    Ok(())
}

fn checked_path(value: &str) -> Result<&Path, CrewImportError> {
    let path = Path::new(value);
    if value.is_empty() || !path.components().all(|part| matches!(part, Component::Normal(_))) {
        return Err(unsafe_data(format!("invalid backup relative path {value:?}")));
    }
    Ok(path)
}

fn hash_file(path: &Path) -> Result<String, CrewImportError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 { break; }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Preserve Crew-only display records using existing upstream render types.
/// The copied legacy SQLite snapshot remains the byte-exact source of truth.
fn convert_legacy_parts(doc: &SessionDoc, chat_id: &str) -> Result<Vec<(String, Vec<u8>)>, CrewImportError> {
    let mut retained = Vec::new();
    let messages = doc.doc().get_list("messages");
    for index in 0..messages.len() {
        let Some(loro::ValueOrContainer::Container(loro::Container::Map(message))) = messages.get(index) else {
            return Err(unsafe_data(format!("malformed transcript row: {chat_id}")));
        };
        // Legacy reconcile_message permits queued/steered -> complete/aborted:
        // neither is a terminal content status. Do not invent a delivery result.
        if message.get("status").is_some_and(|value| matches!(value.get_deep_value().to_json_value().as_str(), Some("queued" | "steered"))) {
            return Err(unsafe_data(format!("legacy queued/steered delivery requires reconciliation: {chat_id}")));
        }
        let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) = message.get("parts") else {
            return Err(unsafe_data(format!("malformed transcript parts: {chat_id}")));
        };
        let peer_original = message.get("peerMessage")
            .filter(|value| !value.get_deep_value().to_json_value().is_null())
            .map(|_| message.get_deep_value().to_json_value());
        for index in 0..parts.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) = parts.get(index) else {
                return Err(unsafe_data(format!("malformed transcript part: {chat_id}")));
            };
            let original = part.get_deep_value().to_json_value();
            match original.get("kind").and_then(Value::as_str) {
                Some("thinking") => {
                    // These are public lifecycle flags, never private reasoning.
                    part.insert("kind", "text")?;
                    part.insert("text", format!("Legacy Crew thinking lifecycle: {}", serde_json::to_string(&original)?))?;
                }
                Some("tool") if original.get("call").and_then(|call| call.get("kind")).and_then(Value::as_str) == Some("agent") => {
                    part.insert("call", LoroValue::from(serde_json::json!({
                        "kind":"unknown", "name":"Legacy Crew Agent", "input":original["call"],
                    })))?;
                }
                _ => {}
            }
        }
        if let Some(original) = peer_original {
            let message_id = original.get("id").and_then(Value::as_str).ok_or_else(|| unsafe_data("peer message without identity"))?;
            let part_id = format!("{message_id}/legacy-peer-provenance");
            if original["parts"].as_array().is_some_and(|parts| parts.iter().any(|part| part.get("id").and_then(Value::as_str) == Some(part_id.as_str()))) {
                return Err(unsafe_data(format!("legacy provenance part ID collision: {message_id}")));
            }
            retained.push((message_id.to_owned(), serde_json::to_vec(&original)?));
            // Append only: existing text, IDs and anchored part indices do not move.
            let part = parts.push_container(loro::LoroMap::new())?;
            part.insert("id", part_id)?;
            part.insert("kind", "text")?;
            part.insert("text", format!("Legacy Crew peer provenance (historical metadata, not authorization): {}", serde_json::to_string(&original["peerMessage"])?))?;
        }
    }
    Ok(retained)
}

/// Import into a nonexistent destination. `principal` MUST come from current
/// verified controller authentication, never from the manifest. No sync or
/// process launch occurs here. Run off the async/UI thread. On failure the
/// new destination is retained for diagnosis and is marked incomplete.
pub fn import_legacy_crew_backup(
    backup: &Path, destination: &Path, principal: &CollaborationPrincipal, device_id: &str,
) -> Result<CrewImportReport, CrewImportError> {
    let backup = backup.canonicalize()?;
    let manifest: CrewBackupManifest = serde_json::from_slice(&std::fs::read(backup.join("crew-backup.json"))?)?;
    if manifest.version != 1 || !manifest.writer_stopped || device_id.is_empty()
        || !Path::new(&manifest.source_profile_root).is_absolute()
        || principal.session_id.is_some() || principal.subject != manifest.owner_subject
        || principal.project_id != manifest.project_id || principal.deployment_id != manifest.deployment_id {
        return Err(unsafe_data("version, stopped-writer confirmation or verified owner/scope mismatch"));
    }
    if destination.exists() { return Err(unsafe_data("destination must be a new separate profile")); }
    let parent = destination.parent().ok_or_else(|| unsafe_data("destination has no parent"))?.canonicalize()?;
    if parent.starts_with(&backup) { return Err(unsafe_data("destination cannot be inside the backup")); }
    let mut actual = BTreeSet::new();
    inventory(&backup, &backup, &mut actual)?;
    actual.remove("crew-backup.json");
    if actual != manifest.files.keys().cloned().collect() || !actual.contains("docs.sqlite3") {
        return Err(unsafe_data("backup file inventory is incomplete"));
    }
    for (relative, expected) in &manifest.files {
        checked_path(relative)?;
        let lower = relative.to_ascii_lowercase();
        if lower.ends_with("-wal") || lower.ends_with("-shm") || lower.contains("credential")
            || lower.contains("token") || lower.contains("auth.json") || lower.contains("keychain")
            || matches!(Path::new(&lower).file_name().and_then(|name| name.to_str()),
                Some("session.json" | "scaffold-session.json" | "scaffold-device-session.json"))
            || lower.contains("device-bootstrap")
            || lower.starts_with("uploads/tmp/") || lower.ends_with(".uploading")
            || lower.split('/').any(|part| part == "agent-accounts")
            || matches!(relative.as_str(), "crew-import-incomplete" | "crew-import-report.json" | "legacy-crew.sqlite3") {
            return Err(unsafe_data(format!("live database sidecar or credential file must not be imported: {relative}")));
        }
        if hash_file(&backup.join(relative))? != *expected {
            return Err(unsafe_data(format!("backup integrity mismatch: {relative}")));
        }
    }
    for (native, relative) in &manifest.native_histories {
        checked_path(relative)?;
        if !relative.starts_with("native-histories/") || !manifest.files.contains_key(relative)
            || native_history_id(&backup.join(relative))? != *native {
            return Err(unsafe_data(format!("native history identity or inventory mismatch: {native}")));
        }
    }
    let database_path = backup.join("docs.sqlite3");
    let database_path = database_path.to_str().ok_or_else(|| unsafe_data("non-UTF8 backup root"))?;
    let database_uri = format!("file:{}?immutable=1",
        percent_encoding::utf8_percent_encode(database_path, percent_encoding::NON_ALPHANUMERIC));
    let db = Connection::open_with_flags(database_uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" { return Err(unsafe_data("SQLite integrity check failed")); }
    let tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |row| row.get::<_, String>(0))?.collect::<Result<BTreeSet<_>, _>>()?;
    for table in &tables {
        if !matches!(table.as_str(), "snapshots" | "processed_commands" | "schema_migrations"
            | "trusted_local_commands" | "local_migrations" | "crew_directory_jobs"
            | "semantic_intents" | "document_scopes" | "pending_nudges") {
            return Err(unsafe_data(format!("unknown legacy table {table}")));
        }
    }
    if tables.contains("pending_nudges") {
        let count: i64 = db.query_row("SELECT COUNT(*) FROM pending_nudges", [], |row| row.get(0))?;
        if count != 0 { return Err(unsafe_data("accepted pending nudges require legacy reconciliation before import")); }
    }
    let snapshots = db.prepare("SELECT doc_id,bytes FROM snapshots ORDER BY doc_id")?
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))?
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let mut claims = db.prepare("SELECT command_id,processed_at FROM processed_commands")?
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut claimed: BTreeSet<String> = claims.iter().map(|(id, _)| id.clone()).collect();
    let mut retained_records = Vec::<(String, String, String, Vec<u8>)>::new();
    let mut scopes = BTreeMap::<String, String>::new();
    if tables.contains("document_scopes") {
        for entry in db.prepare("SELECT doc_id,scope FROM document_scopes")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))? {
            let (id, scope) = entry?;
            let binding: Value = serde_json::from_str(&scope)?;
            if binding.get("project").and_then(Value::as_str) != Some(principal.project_id.as_str())
                || binding.get("principal").and_then(Value::as_str) != Some(principal.subject.as_str())
                || (id != "workspace3" && (binding.get("session").and_then(Value::as_str) != Some(id.as_str())
                    || binding.get("deployment").and_then(Value::as_str) != principal.deployment_id.as_deref())) {
                return Err(unsafe_data(format!("legacy cache scope conflicts with verified import identity: {id}")));
            }
            if scopes.insert(scope, id).is_some() { return Err(unsafe_data("ambiguous document scope binding")); }
        }
    }
    let mut documents = BTreeMap::new();
    for (id, bytes) in &snapshots {
        let doc = LoroDoc::new();
        doc.import(bytes).map_err(|error| unsafe_data(format!("snapshot {id}: {error}")))?;
        documents.insert(id.clone(), doc);
    }
    // A pending semantic journal record must already be represented exactly
    // in the consistent snapshot. Never silently flatten a conflicting intent.
    if tables.contains("semantic_intents") {
        let records = db.prepare("SELECT doc_id,container,record_id,value FROM semantic_intents")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Vec<u8>>(3)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (doc_id, container, key, bytes) in records {
            let doc_id = scopes.get(&doc_id).cloned().unwrap_or(doc_id);
            let record: Value = serde_json::from_slice(&bytes)?;
            retained_records.push((doc_id.clone(), format!("journal/{container}"), key.clone(), bytes));
            let doc = documents.get(&doc_id).ok_or_else(|| unsafe_data(format!("journal has no snapshot: {doc_id}")))?;
            let root = doc.get_deep_value().to_json_value();
            let present = match root.get(&container) {
                Some(Value::Object(rows)) => rows.get(&key),
                Some(Value::Array(rows)) => rows.iter().find(|row| row.get("id").and_then(Value::as_str) == Some(key.as_str())),
                _ => None,
            };
            let desired = record.get("value").filter(|value| !value.is_null());
            if container == "commands" && present != desired {
                if let (Some(current), Some(outcome)) = (present, desired) {
                    let immutable = ["id", "kind", "payload", "issuedBy", "issuedAt", "basedOn", "expiresAt"];
                    if current.get("status").and_then(Value::as_str) == Some("pending")
                        && outcome.get("status").and_then(Value::as_str).is_some_and(|status| status != "pending")
                        && immutable.iter().all(|field| current.get(*field) == outcome.get(*field)) {
                        let status = serde_json::from_value(outcome["status"].clone())?;
                        SessionDoc::from_doc(doc.clone()).set_command_status(&key, status,
                            outcome.get("resolution").and_then(Value::as_str))?;
                        continue;
                    }
                    return Err(unsafe_data(format!("command outcome conflicts with snapshot: {doc_id}/{key}")));
                }
            }
            if record.get("acknowledged").and_then(Value::as_bool) == Some(true) { continue; }
            if present != desired {
                return Err(unsafe_data(format!("unreconciled semantic intent {doc_id}/{container}/{key}")));
            }
        }
    }
    let workspace = documents.remove("workspace3").ok_or_else(|| unsafe_data("missing version-3 legacy workspace snapshot"))?;
    let workspace = workspace.get_deep_value().to_json_value();
    let workspace = workspace.as_object().ok_or_else(|| unsafe_data("invalid workspace root"))?;
    if workspace.get("meta").and_then(|meta| meta.get("schemaVersion")).and_then(Value::as_u64) != Some(3) {
        return Err(unsafe_data("unsupported legacy workspace schema version"));
    }
    let mut registry = RegistryDoc::new(device_id);
    let mut chat_rows = BTreeMap::new();
    for (kind, rows) in workspace {
        if rows.as_object().is_some_and(|rows| rows.is_empty()) { continue; }
        match kind.as_str() {
            "devices" | "spaces" | "chats" => {
                let rows = rows.as_object().ok_or_else(|| unsafe_data(format!("invalid workspace {kind}")))?;
                for (id, row) in rows {
                    let fields: BTreeMap<String, Value> = serde_json::from_value(row.clone())?;
                    if kind == "chats" { chat_rows.insert(id.clone(), fields); }
                    else { registry.import_crew_row(kind, id, fields)?; }
                }
            }
            // Session refs are membership, not alternate chat authority. Exact
            // bindings are required below for every referenced session.
            "sessionRefs" => {
                for row in rows.as_object().ok_or_else(|| unsafe_data("invalid sessionRefs"))?.values() {
                    if row.get("userId").and_then(Value::as_str) != Some(principal.subject.as_str()) {
                        return Err(unsafe_data("foreign session membership requires a separately authenticated import"));
                    }
                    let id = row.get("chatId").and_then(Value::as_str).ok_or_else(|| unsafe_data("session ref without chatId"))?;
                    if !manifest.chats.contains_key(id) { return Err(unsafe_data(format!("session ref has no verified binding: {id}"))); }
                    if let Some(environment) = row.get("environment").filter(|value| !value.is_null()) {
                        let environment: zeron_proto::SessionEnvironment = serde_json::from_value(environment.clone())?;
                        if environment.owner_principal != principal.subject
                            || environment.scope.project_id != principal.project_id
                            || environment.scope.deployment_id != principal.deployment_id
                            || environment.scope.session_id.as_deref() != Some(id) {
                            return Err(unsafe_data(format!("granted session membership requires authenticated reattachment: {id}")));
                        }
                    }
                    if row.get("startup").is_some_and(|value| !value.is_null()) {
                        return Err(unsafe_data(format!("session startup outcome requires reconciliation: {id}")));
                    }
                }
            }
            "sessions" => {} // Ephemeral presence; durable session history is imported below.
            "meta" => {}
            _ => return Err(unsafe_data(format!("unsupported legacy workspace container {kind}"))),
        }
    }
    let mut report = CrewImportReport { version: 1, source_profile_root: manifest.source_profile_root.clone(),
        chat_ids: Vec::new(), native_session_ids: BTreeMap::new(),
        native_histories: manifest.native_histories.clone(),
        command_outcomes: claims.iter().map(|(id, _)| (id.clone(), "unknown".into())).collect(),
        files_sha256: manifest.files.clone(), commands_retained: claims.len(), annotations_retained: 0,
        requires_reauthentication: true };
    let mut command_ids = BTreeSet::new();
    let mut converted = BTreeMap::new();
    for (id, raw) in documents {
        let metadata = manifest.chats.get(&id).ok_or_else(|| unsafe_data(format!("snapshot has no verified chat binding: {id}")))?;
        metadata.validate().map_err(unsafe_data)?;
        if metadata.session_id != id || uuid::Uuid::parse_str(&id).is_err() {
            return Err(unsafe_data(format!("chat ID must preserve the exact public session UUID: {id}")));
        }
        if metadata.owner_subject != principal.subject || metadata.scope.project_id != principal.project_id
            || metadata.scope.deployment_id != principal.deployment_id {
            return Err(unsafe_data(format!("foreign or granted session must be reattached, not imported as owned: {id}")));
        }
        let doc = SessionDoc::from_doc(raw);
        if doc.chat_id().as_deref() != Some(id.as_str()) { return Err(unsafe_data(format!("chat identity mismatch: {id}"))); }
        for (message_id, bytes) in convert_legacy_parts(&doc, &id)? {
            retained_records.push((id.clone(), "messages".into(), message_id, bytes));
        }
        let root = doc.doc().get_deep_value().to_json_value();
        if !matches!(root.get("meta").and_then(|meta| meta.get("schemaVersion")).and_then(Value::as_u64), Some(1 | 2)) {
            return Err(unsafe_data(format!("unsupported legacy transcript schema version: {id}")));
        }
        if let Some(messages) = root.get("messages").and_then(Value::as_array) {
            if doc.read_entries()?.len() != messages.len() { return Err(unsafe_data(format!("malformed transcript entry: {id}"))); }
            for message in messages {
                for part in message.get("parts").and_then(Value::as_array).into_iter().flatten() {
                    match part.get("kind").and_then(Value::as_str) {
                        Some("text" | "reasoning" | "error" | "image" | "visual" | "fork") => {}
                        Some("input") => {
                            let _: Vec<zeron_proto::UserInputQuestion> = serde_json::from_value(
                                part.get("questions").cloned().ok_or_else(|| unsafe_data(format!("input part has no questions: {id}")))?)?;
                        }
                        Some("tool") => {
                            let _: zeron_proto::ToolCall = serde_json::from_value(part.get("call").cloned()
                                .ok_or_else(|| unsafe_data(format!("tool part has no call: {id}")))?)?;
                        }
                        _ => return Err(unsafe_data(format!("unsupported transcript part: {id}"))),
                    }
                }
            }
        }
        if let Some(commands) = root.get("commands").and_then(Value::as_array) {
            for command in commands {
                let command_id = command.get("id").and_then(Value::as_str).ok_or_else(|| unsafe_data("command without id"))?;
                if !command_ids.insert(command_id.to_owned()) {
                    return Err(unsafe_data(format!("ambiguous duplicate command identity: {command_id}")));
                }
                let _: zeron_doc::SessionCommandStatus = serde_json::from_value(command.get("status").cloned()
                    .ok_or_else(|| unsafe_data(format!("command outcome missing: {command_id}")))?)?;
                if command.get("status").and_then(Value::as_str) == Some("pending") && !claimed.contains(command_id) {
                    return Err(unsafe_data(format!("unclaimed pending command must be reconciled before import: {command_id}")));
                }
                let status = command["status"].as_str().ok_or_else(|| unsafe_data("invalid command status"))?;
                report.command_outcomes.insert(command_id.into(),
                    if status == "pending" { "unknown" } else { status }.into());
                retained_records.push((id.clone(), "commands".into(), command_id.into(), serde_json::to_vec(command)?));
                if claimed.insert(command_id.into()) { claims.push((command_id.into(), 0)); }
            }
        }
        if let Some(sessions) = root.get("agentSessions").and_then(Value::as_object) {
            for (session_id, value) in sessions {
                let publication: PublicationRecord = serde_json::from_value(value.clone())?;
                zeron_doc::validate_publication(&publication)?;
                let PublicationValue::AgentSession(session) = publication.value else {
                    return Err(unsafe_data(format!("invalid agent session register: {id}")));
                };
                if session_id != &id || session.session_id != id || session.chat_id != id || session.owner_subject != principal.subject {
                    return Err(unsafe_data(format!("shared or foreign agent membership requires authenticated reattachment: {id}")));
                }
            }
        }
        let mut annotation_ids = BTreeSet::new();
        if let Some(publications) = root.get("publications").and_then(Value::as_array) {
            for row in publications {
                let publication: PublicationRecord = serde_json::from_value(row.get("record").cloned().ok_or_else(|| unsafe_data("invalid publication"))?)?;
                zeron_doc::validate_publication(&publication)?;
                if let PublicationValue::AgentSession(session) = &publication.value {
                    if session.owner_subject != principal.subject || session.chat_id != id || session.session_id != id {
                        return Err(unsafe_data(format!("shared or foreign agent membership requires authenticated reattachment: {id}")));
                    }
                }
                if let PublicationValue::Annotation(annotation) = publication.value {
                    doc.doc().get_map("crewAnnotations").insert(&annotation.id, LoroValue::from(serde_json::to_value(&annotation)?))?;
                    annotation_ids.insert(annotation.id);
                }
            }
        }
        report.annotations_retained += annotation_ids.len();
        let mut row = chat_rows.remove(&id).ok_or_else(|| unsafe_data(format!("session reference has no host row; materialize it through authenticated directory first: {id}")))?;
        if row.get("forkFrom").is_some_and(|value| !value.is_null()) { return Err(unsafe_data(format!("unconsumed native fork requires reconciliation: {id}"))); }
        if let Some(config) = row.get("config").filter(|value| !value.is_null()) {
            let _: zeron_proto::ChatConfig = serde_json::from_value(config.clone())?;
        }
        if let Some(native) = row.get("harnessSessionId").and_then(Value::as_str).filter(|value| !value.is_empty()) {
            report.native_session_ids.insert(id.clone(), native.into());
            if row.get("config").and_then(|config| config.get("harness")).and_then(Value::as_str) != Some("omp") {
                return Err(unsafe_data(format!(
                    "external native-store restore prerequisite: chat={id}, native={native}, cwd={:?}; non-OMP relocation is not implemented, original backup retained",
                    row.get("harnessSessionCwd").or_else(|| row.get("cwd")).and_then(Value::as_str),
                )));
            }
            if !manifest.native_histories.contains_key(native) {
                return Err(unsafe_data(format!("OMP native session needs an explicit journal backup: {native}")));
            }
        }
        row.insert("crew".into(), serde_json::to_value(metadata)?);
        row.insert("roomGen".into(), Value::from(2));
        registry.import_crew_row("chats", &id, row)?;
        doc.doc().get_map("meta").insert("epoch", 2i64)?;
        doc.doc().commit();
        converted.insert(id.clone(), doc.export_snapshot()?);
        report.chat_ids.push(id);
    }
    if !chat_rows.is_empty() || report.chat_ids.len() != manifest.chats.len() {
        return Err(unsafe_data("chat membership, snapshots and explicit bindings differ"));
    }
    report.commands_retained = claims.len();
    if registry.read_chats()?.len() != report.chat_ids.len() {
        return Err(unsafe_data("one or more legacy rows cannot be represented by the upstream chat schema"));
    }
    // All semantic validation completes before creating anything. The original
    // backup remains immutable and contains every original table/outcome byte.
    std::fs::create_dir(destination)?;
    let store = DocsStore::open(destination)?;
    std::fs::write(destination.join("crew-import-incomplete"), b"Do not open until import completes\n")?;
    for relative in manifest.files.keys().filter(|path| path.as_str() != "docs.sqlite3") {
        let output = destination.join(relative);
        if let Some(parent) = output.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::copy(backup.join(relative), &output)?;
        if hash_file(&output)? != manifest.files[relative] { return Err(unsafe_data(format!("copied asset failed integrity: {relative}"))); }
    }
    std::fs::copy(backup.join("docs.sqlite3"), destination.join("legacy-crew.sqlite3"))?;
    if hash_file(&destination.join("legacy-crew.sqlite3"))? != manifest.files["docs.sqlite3"] {
        return Err(unsafe_data("backup database changed during import"));
    }
    {
        let mut conn = store.conn();
        let tx = conn.transaction()?;
        for (doc_id, container, record_id, value) in retained_records {
            tx.execute("INSERT INTO legacy_crew_records(doc_id,container,record_id,value) VALUES (?1,?2,?3,?4)",
                rusqlite::params![doc_id, container, record_id, value])?;
        }
        tx.commit()?;
    }
    store.import_processed_commands(&claims)?;
    for (id, bytes) in converted { store.save_snapshot(&id, &bytes)?; }
    store.save_snapshot(REGISTRY_DOC_ID, &registry.to_bytes()?)?;
    std::fs::write(destination.join("crew-import-report.json"), serde_json::to_vec_pretty(&report)?)?;
    std::fs::remove_file(destination.join("crew-import-incomplete"))?;
    Ok(report)
}
