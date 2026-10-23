//! Explicit, offline Crew backup import. Never open a live legacy profile with
//! the upstream runtime. A hash inventory and verified identity bind the input;
//! publication uses the ordinary personal registry, not the old workspace room.
//! A private SQLite spool bounds import memory to the workspace and current chat;
//! no destination is published until every source record has passed validation.

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
pub use crate::crew_import_recovery::{ImportedRecoveryIntent, imported_recovery_intents};
pub use crate::crew_import_epoch::{OriginalEpochHistories, OriginalEpochProof, imported_original_epoch_histories, validate_original_epoch_bootstrap, imported_original_epoch_proof_json};

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
    /// Exact original attachment reference -> inventoried profile-relative copy.
    #[serde(default)]
    pub attachment_aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub desktop_source: Option<crate::crew_desktop_backup::DesktopSourceProof>,
    #[serde(default)]
    pub original_epoch_histories: BTreeMap<String, zeron_proto::OriginalEpochHistory>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InertBootstrapSnapshot {
    pub snapshot_sha256: String,
    pub disposition: String,
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
    /// Every source table is counted, including non-runtime evidence and jobs.
    #[serde(default)]
    pub source_table_rows: BTreeMap<String, u64>,
    /// Original recovery snapshots remain byte-exact evidence, never live rooms.
    #[serde(default)]
    pub recovery_snapshot_ids: Vec<String>,
    #[serde(default)]
    pub execution_aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub destination_snapshots_sha256: BTreeMap<String, String>,
    #[serde(default)]
    pub source_manifest_sha256: String,
    #[serde(default)]
    pub owner_subject: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub deployment_id: Option<String>,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub legacy_journal_states: BTreeMap<String, String>,
    #[serde(default)]
    pub recovery_intent_ids: BTreeMap<String, String>,
    #[serde(default)]
    pub attachment_aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub desktop_source: Option<crate::crew_desktop_backup::DesktopSourceProof>,
    #[serde(default)]
    pub original_epoch_histories: BTreeMap<String, zeron_proto::OriginalEpochHistory>,
    #[serde(default)]
    pub selected_chat_id: Option<String>,
    #[serde(default)]
    pub inert_bootstrap_snapshots: BTreeMap<String, InertBootstrapSnapshot>,
}

/// Legacy stream/recovery files are evidence only, never installed into the new
/// runtime's active journals directory. Pending requests still block cutover.
fn legacy_journal_states(backup: &Path, manifest: &CrewBackupManifest) -> Result<BTreeMap<String, String>, CrewImportError> {
    use std::io::BufRead;
    let mut states = BTreeMap::new();
    for relative in manifest.files.keys().filter(|path| path.starts_with("legacy-journals/")) {
        let path = backup.join(checked_path(relative)?);
        let state = match path.extension().and_then(|extension| extension.to_str()) {
            Some("context" | "recovery") => {
                let (id, request): (String, Value) = serde_json::from_slice(&std::fs::read(&path)?)?;
                let stem: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' }).collect();
                if path.file_stem().and_then(|name| name.to_str()) != Some(stem.as_str()) {
                    return Err(unsafe_data(format!("legacy journal identity mismatch: {relative}")));
                }
                if path.extension().and_then(|extension| extension.to_str()) == Some("recovery") {
                    if request.is_null() { "retired" } else { "pending-recovery" }
                } else if path.with_extension("jsonl").exists() || path.with_extension("recovery").exists() {
                    "retained-context"
                } else { "unreconciled-context" }
            }
            Some("jsonl") => {
                let mut previous = None;
                let mut terminal = "empty";
                for line in std::io::BufReader::new(std::fs::File::open(&path)?).lines() {
                    let line = line?;
                    if line.trim().is_empty() { continue; }
                    let row: Value = serde_json::from_str(&line)?;
                    let seq = row.get("seq").and_then(Value::as_u64).ok_or_else(|| unsafe_data("legacy event has no sequence"))?;
                    if previous.is_some_and(|previous| seq <= previous) { return Err(unsafe_data("legacy event sequence is not increasing")); }
                    previous = Some(seq);
                    terminal = if row.pointer("/event/type").and_then(Value::as_str) == Some("done")
                        && matches!(row.pointer("/event/status").and_then(Value::as_str), Some("completed" | "interrupted" | "errored")) {
                        "terminal"
                    } else { "pending-stream" };
                }
                terminal
            }
            Some("resume") => {
                std::fs::read_to_string(&path)?.trim().parse::<u32>().map_err(|_| unsafe_data("invalid legacy recovery-attempt counter"))?;
                "retained-attempt-counter"
            }
            _ => return Err(unsafe_data(format!("unknown or incomplete legacy journal sidecar: {relative}"))),
        };
        states.insert(relative.clone(), state.into());
    }
    Ok(states)
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
    let mut paths = BTreeMap::new();
    let root = profile.canonicalize()?;
    for (relative, expected) in report.files_sha256.iter().filter(|(path, _)|
        path.split('/').any(|part| matches!(part, "uploads" | "attachments"))) {
        let relative = checked_path(relative)?;
        let destination = profile.join(relative).canonicalize()?;
        if !destination.starts_with(&root) || destination == root
            || hash_file(&destination)? != *expected {
            return Err(unsafe_data("imported upload is outside its jail or failed integrity"));
        }
        paths.insert(Path::new(&report.source_profile_root).join(relative), destination);
    }
    for (reference, relative) in &report.attachment_aliases {
        checked_attachment_alias(reference, relative, &report.files_sha256)?;
        let destination = profile.join(checked_path(relative)?).canonicalize()?;
        if !destination.starts_with(&root) || destination == root || hash_file(&destination)? != report.files_sha256[relative] {
            return Err(unsafe_data("explicit attachment alias escaped its jail or failed integrity"));
        }
        let original = Path::new(reference).to_path_buf();
        if paths.get(&original).is_some_and(|existing| existing != &destination) {
            return Err(unsafe_data("explicit attachment alias conflicts with copied original path"));
        }
        paths.insert(original, destination);
    }
    Ok(paths.into_iter().collect())
}

fn checked_attachment_alias(reference: &str, relative: &str, files: &BTreeMap<String, String>) -> Result<(), CrewImportError> {
    if !Path::new(reference).is_absolute() || Path::new(reference).components().any(|part| matches!(part, Component::ParentDir))
        || !relative.split('/').any(|part| matches!(part, "uploads" | "attachments"))
        || !files.contains_key(relative) {
        return Err(unsafe_data("attachment alias requires an exact absolute reference and inventoried asset"));
    }
    checked_path(relative)?;
    Ok(())
}


pub(crate) fn native_history_id(path: &Path) -> Result<String, CrewImportError> {
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

/// Resolve immutable source journals for host-only hydrated working copies.
/// Future native writes belong to native-resumes, never these inventoried bytes.
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
        let expected = report.files_sha256.get(&relative).ok_or_else(|| unsafe_data("native history missing from integrity inventory"))?;
        if !root.as_ref().is_some_and(|root| path.starts_with(root) && &path != root)
            || native_history_id(&path)? != id
            || hash_file(&path)? != *expected {
            return Err(unsafe_data("native history identity, integrity or destination jail mismatch"));
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

pub(super) fn hash_file(path: &Path) -> Result<String, CrewImportError> {
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
pub fn convert_legacy_parts(doc: &SessionDoc, chat_id: &str) -> Result<Vec<(String, Vec<u8>)>, CrewImportError> {
    let mut retained = Vec::new();
    convert_parts(doc, chat_id, false, |id, bytes| { retained.push((id.to_owned(), bytes)); Ok(()) })?;
    Ok(retained)
}

fn convert_parts(doc: &SessionDoc, chat_id: &str, original_epoch_history: bool,
    mut retain: impl FnMut(&str, Vec<u8>) -> Result<(), CrewImportError>,
) -> Result<(), CrewImportError> {
    let messages = doc.doc().get_list("messages");
    for index in 0..messages.len() {
        let Some(loro::ValueOrContainer::Container(loro::Container::Map(message))) = messages.get(index) else {
            return Err(unsafe_data(format!("malformed transcript row: {chat_id}")));
        };
        // Legacy reconcile_message permits queued/steered -> complete/aborted:
        // neither is a terminal content status. Do not invent a delivery result.
        if !original_epoch_history && message.get("status").is_some_and(|value| matches!(value.get_deep_value().to_json_value().as_str(), Some("queued" | "steered"))) {
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
            retain(message_id, serde_json::to_vec(&original)?)?;
            // Append only: existing text, IDs and anchored part indices do not move.
            let part = parts.push_container(loro::LoroMap::new())?;
            part.insert("id", part_id)?;
            part.insert("kind", "text")?;
            part.insert("text", format!("Legacy Crew peer provenance (historical metadata, not authorization): {}", serde_json::to_string(&original["peerMessage"])?))?;
        }
    }
    Ok(())
}

/// Import into a nonexistent destination. `principal` MUST come from current
/// verified controller authentication, never from the manifest. No sync or
/// process launch occurs here. Run off the async/UI thread. On failure the
/// new destination is retained for diagnosis and is marked incomplete.
pub fn import_legacy_crew_backup(
    backup: &Path, destination: &Path, principal: &CollaborationPrincipal, device_id: &str,
) -> Result<CrewImportReport, CrewImportError> {
    import_backup(backup, destination, principal, device_id, false, false, None)
}

/// Run the identical inventory and semantic checks without creating a destination.
pub fn validate_legacy_crew_backup(
    backup: &Path, destination: &Path, principal: &CollaborationPrincipal, device_id: &str,
) -> Result<CrewImportReport, CrewImportError> {
    import_backup(backup, destination, principal, device_id, true, false, None)
}

/// Read back the actual destination against a fresh conversion of the immutable
/// source. The persisted import report is never used as evidence.
pub fn verify_legacy_crew_import(
    backup: &Path, destination: &Path, principal: &CollaborationPrincipal, device_id: &str,
) -> Result<CrewImportReport, CrewImportError> {
    import_backup(backup, destination, principal, device_id, false, true, None)
}

/// Fixed-profile proof for an authenticated owner/controller. No caller-selected
/// backup paths: source bytes are the immutable copies retained by this import.
pub fn verify_imported_profile(
    profile: &Path, principal: &CollaborationPrincipal, device_id: &str, chat_id: &str,
) -> Result<CrewImportReport, CrewImportError> {
    if !zeron_proto::is_cloud_session_id(chat_id) { return Err(unsafe_data("selected import proof requires a canonical session UUID")); }
    import_backup(profile, profile, principal, device_id, false, true, Some(chat_id))
}

/// A failed preflight still inventories every table and snapshot and groups all
/// semantic journal records by container/outcome; no transcript payload escapes.
pub fn preflight_legacy_crew_backup(
    backup: &Path, destination: &Path, principal: &CollaborationPrincipal, device_id: &str,
) -> Result<Value, CrewImportError> {
    let backup = backup.canonicalize()?;
    let database = backup.join("docs.sqlite3");
    let uri = format!("file:{}?immutable=1", percent_encoding::utf8_percent_encode(
        database.to_str().ok_or_else(|| unsafe_data("non-UTF8 backup root"))?, percent_encoding::NON_ALPHANUMERIC));
    let db = Connection::open_with_flags(uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)?;
    let tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?
        .query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    let mut counts = BTreeMap::new();
    for table in &tables {
        let identifier = table.replace('"', "\"\"");
        let count: u64 = db.query_row(&format!("SELECT COUNT(*) FROM \"{identifier}\""), [], |row| row.get(0))?;
        counts.insert(table.clone(), count);
    }
    let mut snapshots = BTreeMap::new();
    if counts.contains_key("snapshots") {
        for id in db.prepare("SELECT doc_id FROM snapshots ORDER BY doc_id")?.query_map([], |row| row.get::<_, String>(0))? {
            let id = id?;
            let kind = if id == "workspace3" { "workspace" }
                else if id.starts_with("recovery-original/") { "recovery-evidence" }
                else if id.contains("::session::") { "execution-alias" }
                else if id.starts_with("local-chat-") { "local-history" }
                else if uuid::Uuid::parse_str(&id).is_ok() { "public-session" }
                else { "unclassified" };
            snapshots.insert(id, kind);
        }
    }
    let mut semantic = BTreeMap::<String, BTreeMap<String, u64>>::new();
    if counts.contains_key("semantic_intents") {
        for row in db.prepare("SELECT container,value FROM semantic_intents ORDER BY doc_id,container,record_id")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))? {
            let (container, bytes) = row?;
            let record = serde_json::from_slice::<Value>(&bytes);
            let status = match &record {
                Ok(record) if container == "commands" => record.get("value").and_then(|value| value.get("status"))
                    .and_then(Value::as_str).unwrap_or("missing-outcome"),
                Ok(record) if record.get("acknowledged").and_then(Value::as_bool) == Some(true) => "acknowledged",
                Ok(_) => "unacknowledged",
                Err(_) => "malformed",
            };
            *semantic.entry(container).or_default().entry(status.to_owned()).or_default() += 1;
        }
    }
    let journal_inventory = std::fs::read(backup.join("crew-backup.json")).map_err(CrewImportError::from)
        .and_then(|bytes| serde_json::from_slice::<CrewBackupManifest>(&bytes).map_err(Into::into))
        .and_then(|manifest| legacy_journal_states(&backup, &manifest));
    let journals = match journal_inventory {
        Ok(states) => serde_json::json!({"states":states}),
        Err(error) => serde_json::json!({"error":error.to_string()}),
    };
    let result = validate_legacy_crew_backup(&backup, destination, principal, device_id);
    Ok(match result {
        Ok(report) => serde_json::json!({"valid":true,"report":report,"sourceTableRows":counts,"snapshots":snapshots,"semanticRecords":semantic,"legacyJournals":journals}),
        Err(error) => serde_json::json!({"valid":false,"error":error.to_string(),"sourceTableRows":counts,"snapshots":snapshots,"semanticRecords":semantic,"legacyJournals":journals}),
    })
}


fn import_backup(
    backup: &Path, destination: &Path, principal: &CollaborationPrincipal, device_id: &str,
    validate_only: bool, verify_only: bool, selected_chat: Option<&str>,
) -> Result<CrewImportReport, CrewImportError> {
    let backup = backup.canonicalize()?;
    let embedded = selected_chat.is_some();
    let manifest_bytes = std::fs::read(backup.join(if embedded { "crew-import-source.json" } else { "crew-backup.json" }))?;
    let manifest: CrewBackupManifest = serde_json::from_slice(&manifest_bytes)?;
    if manifest.version != 1 || !manifest.writer_stopped || device_id.is_empty()
        || !Path::new(&manifest.source_profile_root).is_absolute()
        || principal.session_id.is_some() || principal.subject != manifest.owner_subject
        || principal.project_id != manifest.project_id || principal.deployment_id != manifest.deployment_id {
        return Err(unsafe_data("version, stopped-writer confirmation or verified owner/scope mismatch"));
    }
    if let Some(id) = selected_chat {
        let chat = manifest.chats.get(id).ok_or_else(|| unsafe_data("selected session is not in this imported profile"))?;
        if chat.owner_subject != principal.subject || chat.scope.project_id != principal.project_id
            || chat.scope.deployment_id != principal.deployment_id || chat.session_id != id {
            return Err(unsafe_data("selected session differs from verified owner/scope"));
        }
    }
    if !verify_only && destination.exists() { return Err(unsafe_data("destination must be a new separate profile")); }
    if verify_only && (!destination.is_dir() || destination.join("crew-import-incomplete").exists()) {
        return Err(unsafe_data("readback requires a completed imported destination"));
    }
    let parent = destination.parent().ok_or_else(|| unsafe_data("destination has no parent"))?.canonicalize()?;
    if !embedded && parent.starts_with(&backup) { return Err(unsafe_data("destination cannot be inside the backup")); }
    if !embedded {
        let mut actual = BTreeSet::new();
        inventory(&backup, &backup, &mut actual)?;
        actual.remove("crew-backup.json");
        if actual != manifest.files.keys().cloned().collect() || !actual.contains("docs.sqlite3") {
            return Err(unsafe_data("backup file inventory is incomplete"));
        }
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
            || matches!(relative.as_str(), "crew-import-incomplete" | "crew-import-report.json" | "crew-import-source.json" | "legacy-crew.sqlite3") {
            return Err(unsafe_data(format!("live database sidecar or credential file must not be imported: {relative}")));
        }
        let source = backup.join(if embedded && relative == "docs.sqlite3" { "legacy-crew.sqlite3" } else { relative });
        if !source.canonicalize()?.starts_with(&backup) || hash_file(&source)? != *expected {
            return Err(unsafe_data(format!("backup integrity mismatch: {relative}")));
        }
    }
    for (reference, relative) in &manifest.attachment_aliases {
        checked_attachment_alias(reference, relative, &manifest.files)?;
        if let Ok(implicit) = Path::new(reference).strip_prefix(&manifest.source_profile_root)
            && let Some(implicit) = implicit.to_str()
            && manifest.files.contains_key(implicit) && implicit != relative {
            return Err(unsafe_data("attachment alias conflicts with an inventoried original path"));
        }
    }
    for (native, relative) in &manifest.native_histories {
        checked_path(relative)?;
        if !relative.starts_with("native-histories/") || !manifest.files.contains_key(relative)
            || native_history_id(&backup.join(relative))? != *native {
            return Err(unsafe_data(format!("native history identity or inventory mismatch: {native}")));
        }
    }
    if let Some(proof) = &manifest.desktop_source { proof.validate_evidence(&backup, &manifest.files, principal)?; }
    let original_epochs = crate::crew_import_epoch::validate(&backup, &manifest, principal)?;
    let mut legacy_journal_states = legacy_journal_states(&backup, &manifest)?;
    let database_path = backup.join(if embedded { "legacy-crew.sqlite3" } else { "docs.sqlite3" });
    let database_path = database_path.to_str().ok_or_else(|| unsafe_data("non-UTF8 backup root"))?;
    let database_uri = format!("file:{}?immutable=1",
        percent_encoding::utf8_percent_encode(database_path, percent_encoding::NON_ALPHANUMERIC));
    let db = Connection::open_with_flags(database_uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)?;
    db.execute_batch("PRAGMA cache_size=-4096; PRAGMA temp_store=FILE;")?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" { return Err(unsafe_data("SQLite integrity check failed")); }
    let tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |row| row.get::<_, String>(0))?.collect::<Result<BTreeSet<_>, _>>()?;
    for table in &tables {
        if !matches!(table.as_str(), "snapshots" | "processed_commands" | "schema_migrations"
            | "trusted_local_commands" | "local_migrations" | "crew_directory_jobs"
            | "semantic_intents" | "document_scopes" | "pending_nudges" | "session_startups") {
            return Err(unsafe_data(format!("unknown legacy table {table}")));
        }
    }
    let mut source_table_rows = BTreeMap::new();
    for table in &tables {
        // Identifiers have already passed the closed table-name allowlist.
        let count: u64 = db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))?;
        source_table_rows.insert(table.clone(), count);
    }
    if source_table_rows.get("session_startups").is_some_and(|count| *count != 0) {
        return Err(unsafe_data("accepted/cancelled session startups require legacy reconciliation before import"));
    }
    if tables.contains("pending_nudges") {
        let count: i64 = db.query_row("SELECT COUNT(*) FROM pending_nudges", [], |row| row.get(0))?;
        if count != 0 { return Err(unsafe_data("accepted pending nudges require legacy reconciliation before import")); }
    }
    let mut claims = db.prepare("SELECT command_id,processed_at FROM processed_commands")?
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut claimed: BTreeSet<String> = claims.iter().map(|(id, _)| id.clone()).collect();
    let mut scopes = BTreeMap::<String, String>::new();
    if tables.contains("document_scopes") {
        for entry in db.prepare("SELECT doc_id,scope FROM document_scopes")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))? {
            let (id, scope) = entry?;
            let binding: Value = serde_json::from_str(&scope)?;
            let canonical = crate::crew_import_aliases::canonical_execution_id(&id).unwrap_or(&id);
            if let Some(proof) = &manifest.desktop_source {
                proof.validate_source_scope(&id, &binding, principal)?;
            } else if binding.get("project").and_then(Value::as_str) != Some(principal.project_id.as_str())
                || binding.get("principal").and_then(Value::as_str) != Some(principal.subject.as_str())
                || (id != "workspace3" && ((!matches!(binding.get("session").and_then(Value::as_str), Some(session) if session == id || session == canonical))
                    || binding.get("deployment").and_then(Value::as_str) != principal.deployment_id.as_deref())) {
                return Err(unsafe_data(format!("legacy cache scope conflicts with verified import identity: {id}")));
            }
            if let Some(previous) = scopes.insert(scope, canonical.to_owned())
                && previous != canonical { return Err(unsafe_data("ambiguous document scope binding")); }
        }
    }
    let mut recovery_snapshot_ids = Vec::new();
    let mut associated_ids: BTreeSet<String> = manifest.native_histories.keys().chain(manifest.chats.keys()).cloned().collect();
    if let Some(proof) = &manifest.desktop_source { associated_ids.extend(proof.source_witnesses.keys().cloned()); }
    let inert_bootstraps = crate::crew_import_aliases::inert_bootstrap_snapshots_from_store(&db, &associated_ids)?;
    if manifest.desktop_source.as_ref().is_some_and(|proof| proof.inert_bootstrap_snapshots != inert_bootstraps) {
        return Err(unsafe_data("inert bootstrap disposition differs from original source evidence"));
    }
    let temporary = tempfile::tempdir()?;
    let prepared = crate::crew_import_aliases::prepare_execution_snapshots(&db, &temporary.path().canonicalize()?, &manifest, principal,
        &inert_bootstraps.keys().cloned().collect())?;
    let staged = &prepared.connection;
    staged.execute_batch("CREATE TABLE retained_records(doc_id TEXT NOT NULL,container TEXT NOT NULL,record_id TEXT NOT NULL,value BLOB NOT NULL,
            PRIMARY KEY(doc_id,container,record_id)) STRICT;
        CREATE TABLE converted_snapshots(doc_id TEXT PRIMARY KEY,bytes BLOB NOT NULL) STRICT;")?;
    staged.execute_batch("BEGIN")?;
    let execution_aliases = &prepared.execution_aliases;
    let mut inert_bootstrap_snapshots = BTreeMap::new();
    for (id, snapshot_sha256) in inert_bootstraps {
        inert_bootstrap_snapshots.insert(id, InertBootstrapSnapshot { snapshot_sha256, disposition: "inert-bootstrap".into() });
    }
    // Original bytes stay inert evidence; only staged canonical snapshots execute.
    for entry in db.prepare("SELECT doc_id FROM snapshots ORDER BY doc_id")?
        .query_map([], |row| row.get::<_, String>(0))? {
        let id = entry?;
        let container = if id.starts_with("recovery-original/") {
            recovery_snapshot_ids.push(id.clone());
            "snapshot"
        } else if inert_bootstrap_snapshots.contains_key(&id) { "inert-bootstrap" }
        else if execution_aliases.contains_key(&id) { "snapshot" }
        else { continue; };
        let bytes: Vec<u8> = db.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [&id], |row| row.get(0))?;
        staged.execute("INSERT INTO retained_records VALUES (?1,?2,?1,?3)", rusqlite::params![id, container, bytes])?;
    }
    for (path, state) in &mut legacy_journal_states {
        let stem = Path::new(path).file_stem().and_then(|name| name.to_str()).unwrap_or("");
        let mut parts = stem.split("__session__");
        let canonical = parts.next().unwrap_or("");
        if original_epochs.contains_key(canonical) && parts.all(|part| part == canonical) {
            *state = "original-epoch-inert".into();
        }
    }
    if let Some((path, state)) = legacy_journal_states.iter().find(|(_, state)|
        matches!(state.as_str(), "pending-stream" | "unreconciled-context")) {
        return Err(unsafe_data(format!("legacy execution journal requires reconciliation before import: {path} ({state})")));
    }
    let recovery_intent_ids = crate::crew_import_recovery::prepare(&backup, &manifest, principal, device_id,
        &mut legacy_journal_states, staged).map_err(|error|
            unsafe_data(format!("legacy execution journal requires reconciliation before import: {error}")))?;
    // A pending semantic journal record must already be represented exactly
    // in the consistent snapshot. Never silently flatten a conflicting intent.
    if tables.contains("semantic_intents") {
        let mut statement = db.prepare("SELECT doc_id,container,record_id,value FROM semantic_intents ORDER BY doc_id,container,record_id")?;
        let records = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Vec<u8>>(3)?)))?;
        // Preserve source order, including scope-string rows that revisit a chat.
        // Persist outcomes before eviction; materialize the current root only once.
        let mut cached: Option<(String, LoroDoc, Value, bool)> = None;
        for entry in records {
            let (doc_id, container, key, bytes) = entry?;
            let original_doc_id = scopes.get(&doc_id).cloned().unwrap_or(doc_id);
            let doc_id = execution_aliases.get(&original_doc_id).cloned().unwrap_or_else(|| original_doc_id.clone());
            let mut record: Value = serde_json::from_slice(&bytes)?;
            staged.execute("INSERT INTO retained_records VALUES (?1,?2,?3,?4)",
                rusqlite::params![original_doc_id, format!("journal/{container}"), key, bytes])?;
            if original_epochs.contains_key(&doc_id) { continue; } // Exact old intent bytes stay inert, never replayed in a new epoch.
            for (alias, canonical) in execution_aliases {
                if let Some(value) = record.get_mut("value").filter(|value| !value.is_null()) {
                    crate::crew_import_aliases::normalize_execution_record(&container, value, alias, canonical)?;
                }
            }
            let key = if let Some(value) = record.get("value").filter(|value| !value.is_null()) {
                match container.as_str() {
                    "publications" => value.get("id").and_then(Value::as_str).unwrap_or(&key).to_owned(),
                    "agentSessions" => value.get("value").and_then(|value| value.get("sessionId")).and_then(Value::as_str).unwrap_or(&key).to_owned(),
                    "sessionRefs" => match (value.get("userId").and_then(Value::as_str), value.get("chatId").and_then(Value::as_str)) {
                        (Some(user), Some(chat)) => format!("{}:{user}:{chat}", user.len()),
                        _ => return Err(unsafe_data("session ref journal has no exact membership identity")),
                    },
                    "chats" | "sessions" | "worktreeDeletions" => execution_aliases.get(&key).cloned().unwrap_or(key),
                    _ => key,
                }
            } else {
                if key.contains("::session::") && !execution_aliases.contains_key(&key) {
                    return Err(unsafe_data("private journal deletion requires exact legacy reconciliation"));
                }
                execution_aliases.get(&key).cloned().unwrap_or(key)
            };
            if cached.as_ref().is_none_or(|(id, _, _, _)| id != &doc_id) {
                if let Some((id, doc, _, dirty)) = cached.take() {
                    if dirty {
                        staged.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id=?2",
                            rusqlite::params![SessionDoc::from_doc(doc).export_snapshot()?, id])?;
                    }
                }
                let doc = crate::crew_import_aliases::load_execution_doc(staged, &doc_id)?;
                let root = doc.get_deep_value().to_json_value();
                cached = Some((doc_id.clone(), doc, root, false));
            }
            let (_, doc, root, dirty) = cached.as_mut().expect("current journal snapshot loaded");
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
                        // Match set_command_status: absent resolution leaves it unchanged.
                        let current = match root.get_mut(&container) {
                            Some(Value::Object(rows)) => rows.get_mut(&key),
                            Some(Value::Array(rows)) => rows.iter_mut().find(|row| row.get("id").and_then(Value::as_str) == Some(key.as_str())),
                            _ => None,
                        }.expect("reconciled command exists");
                        current["status"] = outcome["status"].clone();
                        if let Some(resolution) = outcome.get("resolution").and_then(Value::as_str) {
                            current["resolution"] = Value::from(resolution);
                        }
                        *dirty = true;
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
        if let Some((id, doc, _, true)) = cached {
            staged.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id=?2",
                rusqlite::params![SessionDoc::from_doc(doc).export_snapshot()?, id])?;
        }
    }
    let workspace = crate::crew_import_aliases::load_execution_doc(staged, "workspace3")?;
    let raw_devices = workspace.get_map("devices");
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
                    let mut fields: BTreeMap<String, Value> = serde_json::from_value(row.clone())?;
                    if kind == "devices" {
                        let legacy_namespace = fields.get("namespaceDevboxId").or_else(|| fields.get("namespaceInstanceId"));
                        let legacy_boat = fields.get("boatSandboxId");
                        let legacy = match (legacy_namespace, legacy_boat) {
                            (Some(_), Some(_)) => return Err(unsafe_data(format!("conflicting legacy host providers: {id}"))),
                            (Some(value), None) => Some(("namespace", value.clone())),
                            (None, Some(value)) => Some(("boat", value.clone())),
                            _ => None,
                        };
                        if let Some((provider, binding)) = legacy {
                            if fields.get("environment").filter(|value| !value.is_null()).is_some_and(|value| value.as_str() != Some(provider)) {
                                return Err(unsafe_data(format!("legacy host environment conflicts with provider binding: {id}")));
                            }
                            if binding.as_str().is_none_or(str::is_empty) {
                                return Err(unsafe_data(format!("invalid legacy host binding: {id}")));
                            }
                            for (field, value) in [("devboxProvider", Value::from(provider)), ("devboxId", binding)] {
                                if let Some(existing) = fields.get(field) {
                                    if existing != &value { return Err(unsafe_data(format!("conflicting legacy host binding: {id}"))); }
                                } else {
                                    // A deleted neutral binding must not be revived from stale legacy fields.
                                    if let Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) = raw_devices.get(id)
                                        && row.get_last_editor(field).is_some() {
                                        return Err(unsafe_data(format!("cleared host binding requires reconciliation: {id}")));
                                    }
                                    fields.insert(field.into(), value);
                                }
                            }
                        }
                        for field in ["namespaceDevboxId", "namespaceInstanceId", "boatSandboxId", "environment"] { fields.remove(field); }
                    }
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
        requires_reauthentication: true, source_table_rows, recovery_snapshot_ids, execution_aliases: execution_aliases.clone(),
        destination_snapshots_sha256: BTreeMap::new(),
        source_manifest_sha256: format!("{:x}", Sha256::digest(&manifest_bytes)),
        owner_subject: principal.subject.clone(), project_id: principal.project_id.clone(),
        deployment_id: principal.deployment_id.clone(), device_id: device_id.into(), legacy_journal_states, recovery_intent_ids,
        attachment_aliases: manifest.attachment_aliases.clone(), desktop_source: manifest.desktop_source.clone(),
        original_epoch_histories: manifest.original_epoch_histories.clone(), selected_chat_id: None, inert_bootstrap_snapshots };
    let mut command_ids = BTreeSet::new();
    for entry in staged.prepare("SELECT doc_id FROM snapshots WHERE doc_id!='workspace3' ORDER BY doc_id")?
        .query_map([], |row| row.get::<_, String>(0))? {
        let id = entry?;
        let raw = crate::crew_import_aliases::load_execution_doc(staged, &id)?;
        let metadata = manifest.chats.get(&id).ok_or_else(|| unsafe_data(format!("snapshot has no verified chat binding: {id}")))?;
        metadata.validate().map_err(unsafe_data)?;
        let original_epoch = original_epochs.get(&id);
        if metadata.original_epoch_history.as_ref() != manifest.original_epoch_histories.get(&id) {
            return Err(unsafe_data("original epoch metadata has no exact source evidence"));
        }
        let local_history = zeron_proto::is_local_history_chat(&id);
        let witnessed_unscoped_local = local_history && metadata.scope.deployment_id.is_none()
            && manifest.desktop_source.as_ref().is_some_and(|proof| proof.document_scopes.get(&id)
                .is_some_and(|scope| scope.get("deployment").is_some_and(Value::is_null)));
        if metadata.session_id != id || (!local_history && uuid::Uuid::parse_str(&id).is_err()) {
            return Err(unsafe_data(format!("chat ID must preserve a public session UUID or verified local-history identity: {id}")));
        }
        if metadata.owner_subject != principal.subject || metadata.scope.project_id != principal.project_id
            || (metadata.scope.deployment_id != principal.deployment_id && !witnessed_unscoped_local) {
            return Err(unsafe_data(format!("foreign or granted session must be reattached, not imported as owned: {id}")));
        }
        let doc = SessionDoc::from_doc(raw);
        if doc.chat_id().as_deref() != Some(id.as_str()) { return Err(unsafe_data(format!("chat identity mismatch: {id}"))); }
        convert_parts(&doc, &id, original_epoch.is_some(), |message_id, bytes| {
            staged.execute("INSERT INTO retained_records VALUES (?1,'messages',?2,?3)",
                rusqlite::params![id, message_id, bytes])?;
            Ok(())
        })?;
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
        let mut original_outcomes = BTreeMap::new();
        if let Some(commands) = root.get("commands").and_then(Value::as_array) {
            for command in commands {
                let command_id = command.get("id").and_then(Value::as_str).ok_or_else(|| unsafe_data("command without id"))?;
                if !command_ids.insert(command_id.to_owned()) {
                    return Err(unsafe_data(format!("ambiguous duplicate command identity: {command_id}")));
                }
                if original_epoch.is_none() {
                    let _: zeron_doc::SessionCommandStatus = serde_json::from_value(command.get("status").cloned()
                        .ok_or_else(|| unsafe_data(format!("command outcome missing: {command_id}")))?)?;
                }
                if original_epoch.is_none() && command.get("status").and_then(Value::as_str) == Some("pending") && !claimed.contains(command_id) {
                    return Err(unsafe_data(format!("unclaimed pending command must be reconciled before import: {command_id}")));
                }
                let status = command["status"].as_str().ok_or_else(|| unsafe_data("invalid command status"))?;
                original_outcomes.insert(command_id.to_owned(), status.to_owned());
                report.command_outcomes.insert(command_id.into(),
                    if status == "pending" && original_epoch.is_none() { "unknown" } else { status }.into());
                staged.execute("INSERT INTO retained_records VALUES (?1,'commands',?2,?3)",
                    rusqlite::params![id, command_id, serde_json::to_vec(command)?])?;
                if claimed.insert(command_id.into()) { claims.push((command_id.into(), 0)); }
            }
        }
        if let Some(proof) = original_epoch { crate::crew_import_epoch::verify_outcomes(proof, &original_outcomes)?; }
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
        if original_epoch.is_none() && row.get("forkFrom").is_some_and(|value| !value.is_null()) { return Err(unsafe_data(format!("unconsumed native fork requires reconciliation: {id}"))); }
        if let Some(config) = row.get("config").filter(|value| !value.is_null()) {
            let _: zeron_proto::ChatConfig = serde_json::from_value(config.clone())?;
        }
        if local_history && (row.get("harnessSessionId").and_then(Value::as_str).is_none_or(str::is_empty)
            || row.get("harnessSessionCwd").or_else(|| row.get("cwd")).and_then(Value::as_str).is_none_or(str::is_empty)
            || metadata.environment.as_ref().is_some_and(|environment| !matches!(&environment.source, zeron_proto::SessionEnvironmentSource::Local))) {
            return Err(unsafe_data(format!("local history requires its original native resume, cwd and local environment: {id}")));
        }
        if let Some(proof) = original_epoch {
            if row.get("harnessSessionId").and_then(Value::as_str).filter(|value| !value.is_empty()) != proof.native_session_id.as_deref()
                || row.get("deviceId").and_then(Value::as_str) != Some(proof.scope.device_id.as_str()) {
                return Err(unsafe_data("original epoch native/host association differs from evidence"));
            }
        }
        if let Some(native) = row.get("harnessSessionId").and_then(Value::as_str).filter(|value| !value.is_empty()) {
            report.native_session_ids.insert(id.clone(), native.into());
            if original_epoch.is_none() && row.get("config").and_then(|config| config.get("harness")).and_then(Value::as_str) != Some("omp") {
                return Err(unsafe_data(format!(
                    "external native-store restore prerequisite: chat={id}, native={native}, cwd={:?}; non-OMP relocation is not implemented, original backup retained",
                    row.get("harnessSessionCwd").or_else(|| row.get("cwd")).and_then(Value::as_str),
                )));
            }
            if original_epoch.is_none() && !manifest.native_histories.contains_key(native) {
                return Err(unsafe_data(format!("OMP native session needs an explicit journal backup: {native}")));
            }
        }
        if local_history {
            // Account binding is import evidence, not permission to publish a
            // non-UUID native-history alias into an authenticated cloud room.
            row.remove("crew");
        } else {
            row.insert("crew".into(), serde_json::to_value(metadata)?);
        }
        row.insert("roomGen".into(), Value::from(2));
        registry.import_crew_row("chats", &id, row)?;
        doc.doc().get_map("meta").insert("epoch", 2i64)?;
        doc.doc().commit();
        staged.execute("INSERT INTO converted_snapshots VALUES (?1,?2)", rusqlite::params![id, doc.export_snapshot()?])?;
        report.chat_ids.push(id);
    }
    if !chat_rows.is_empty() || report.chat_ids.len() != manifest.chats.len() {
        return Err(unsafe_data("chat membership, snapshots and explicit bindings differ"));
    }
    report.commands_retained = claims.len();
    if registry.read_chats()?.len() != report.chat_ids.len() {
        return Err(unsafe_data("one or more legacy rows cannot be represented by the upstream chat schema"));
    }
    staged.execute_batch("COMMIT")?;
    if verify_only {
        let destination = destination.canonicalize()?;
        if !embedded && destination.starts_with(&backup) { return Err(unsafe_data("readback destination overlaps backup")); }
        if std::fs::read(destination.join("crew-import-source.json"))? != manifest_bytes {
            return Err(unsafe_data("destination source manifest bytes differ"));
        }
        // The report is not proof, but its aliases drive runtime attachment and
        // native recovery. They must match independently reconstructed evidence.
        let operational: CrewImportReport = serde_json::from_slice(&std::fs::read(destination.join("crew-import-report.json"))?)?;
        if serde_json::to_value(operational)? != serde_json::to_value(&report)? {
            return Err(unsafe_data("destination operational import metadata differs from source evidence"));
        }
        for (relative, expected) in &manifest.files {
            let path = destination.join(if relative == "docs.sqlite3" { "legacy-crew.sqlite3" } else { relative });
            if !path.canonicalize()?.starts_with(&destination) || hash_file(&path)? != *expected {
                return Err(unsafe_data(format!("destination copied file differs: {relative}")));
            }
        }
        let actual = Connection::open_with_flags(destination.join("docs.sqlite3"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        if selected_chat.is_none() {
            let actual_ids = actual.prepare("SELECT doc_id FROM snapshots ORDER BY doc_id")?
                .query_map([], |row| row.get::<_, String>(0))?.collect::<Result<BTreeSet<_>, _>>()?;
            let expected_ids = report.chat_ids.iter().cloned().chain([REGISTRY_DOC_ID.to_owned()]).collect::<BTreeSet<_>>();
            if actual_ids != expected_ids { return Err(unsafe_data("destination snapshot membership differs")); }
        }
        let mut selected_commands = BTreeSet::new();
        let mut selected_annotations = 0;
        for entry in staged.prepare("SELECT doc_id,bytes FROM converted_snapshots WHERE ?1 IS NULL OR doc_id=?1 ORDER BY doc_id")?
            .query_map([selected_chat], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))? {
            let (id, expected) = entry?;
            let bytes: Vec<u8> = actual.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [&id], |row| row.get(0))?;
            let actual_doc = LoroDoc::new();
            if actual_doc.import(&bytes)?.pending.is_some() || actual_doc.is_detached() || !crate::crew_import_aliases::state_matches_oplog(&actual_doc, &actual_doc.oplog_vv())? {
                return Err(unsafe_data(format!("destination has unresolved causal operations: {id}")));
            }
            let expected_doc = LoroDoc::new(); expected_doc.import(&expected)?;
            if actual_doc.get_deep_value() != expected_doc.get_deep_value() {
                return Err(unsafe_data(format!("destination transcript/semantic state differs: {id}")));
            }
            report.destination_snapshots_sha256.insert(id.clone(), format!("{:x}", Sha256::digest(&bytes)));
            if selected_chat.is_some() {
                for command in expected_doc.get_list("commands").get_deep_value().to_json_value().as_array().into_iter().flatten() {
                    if let Some(id) = command.get("id").and_then(Value::as_str) { selected_commands.insert(id.to_owned()); }
                }
                selected_annotations = expected_doc.get_map("crewAnnotations").len();
            }
        }
        let registry_bytes: Vec<u8> = actual.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [REGISTRY_DOC_ID], |row| row.get(0))?;
        let actual_registry = RegistryDoc::from_bytes(&registry_bytes, device_id)?;
        let registry_matches = if let Some(id) = selected_chat {
            let mut current = actual_registry.chat(id)?.ok_or_else(|| unsafe_data("selected registry chat is absent"))?;
            let expected = registry.chat(id)?.ok_or_else(|| unsafe_data("selected source registry chat is absent"))?;
            // Viewing history may advance the read marker; no config, identity,
            // transcript freshness, or authority field is exempted.
            if current.last_seen_at < expected.last_seen_at { return Err(unsafe_data("selected read marker regressed")); }
            current.last_seen_at = expected.last_seen_at;
            serde_json::to_value(current)? == serde_json::to_value(expected)?
        } else { serde_json::to_value(actual_registry.read_all()?)? == serde_json::to_value(registry.read_all()?)? };
        if !registry_matches { return Err(unsafe_data("destination registry identity/config/native association differs")); }
        report.destination_snapshots_sha256.insert(REGISTRY_DOC_ID.into(), format!("{:x}", Sha256::digest(&registry_bytes)));
        if selected_chat.is_some() {
            for (id, timestamp) in claims.iter().filter(|(id, _)| selected_commands.contains(id)) {
                let actual_timestamp: i64 = actual.query_row("SELECT processed_at FROM processed_commands WHERE command_id=?1", [id], |row| row.get(0))?;
                if actual_timestamp != *timestamp { return Err(unsafe_data("selected durable command claim differs")); }
            }
        } else {
            let actual_claims = actual.prepare("SELECT command_id,processed_at FROM processed_commands ORDER BY command_id")?
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?.collect::<Result<BTreeMap<_, _>, _>>()?;
            if actual_claims != claims.into_iter().collect() { return Err(unsafe_data("destination durable command claims differ")); }
        }
        let mut actual_statement = actual.prepare("SELECT doc_id,container,record_id,value FROM legacy_crew_records WHERE ?1 IS NULL OR doc_id=?1 OR doc_id LIKE ?1 || '::session::%' ORDER BY doc_id,container,record_id")?;
        let mut actual_records = actual_statement.query_map([selected_chat], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Vec<u8>>(3)?)))?;
        for entry in staged.prepare("SELECT doc_id,container,record_id,value FROM retained_records ORDER BY doc_id,container,record_id")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Vec<u8>>(3)?)))? {
            let expected = entry?;
            if selected_chat.is_some_and(|selected| expected.0 != selected
                && !report.execution_aliases.get(&expected.0).is_some_and(|canonical| canonical == selected)) { continue; }
            if actual_records.next().transpose()?.as_ref() != Some(&expected) {
                return Err(unsafe_data("destination immutable outcomes/evidence differ"));
            }
        }
        if actual_records.next().transpose()?.is_some() { return Err(unsafe_data("destination immutable outcomes/evidence differ")); }
        if let Some(id) = selected_chat {
            report.selected_chat_id = Some(id.into());
            report.chat_ids.retain(|chat| chat == id);
            report.native_session_ids.retain(|chat, _| chat == id);
            report.native_histories.retain(|native, _| report.native_session_ids.values().any(|id| id == native));
            report.command_outcomes.retain(|command, _| selected_commands.contains(command));
            report.commands_retained = selected_commands.len();
            report.annotations_retained = selected_annotations;
            report.execution_aliases.retain(|_, canonical| canonical == id);
            report.original_epoch_histories.retain(|chat, _| chat == id);
            report.recovery_intent_ids.retain(|chat, _| chat == id);
            report.recovery_snapshot_ids.clear();
            report.source_table_rows.clear();
            report.legacy_journal_states.clear();
            report.attachment_aliases.clear();
            report.inert_bootstrap_snapshots.clear();
            report.desktop_source = None;
            let proof = original_epochs.get(id);
            let marker = report.original_epoch_histories.get(id);
            report.files_sha256.retain(|path, hash| path == "docs.sqlite3" || report.native_histories.values().any(|native| native == path)
                || proof.is_some_and(|proof| &proof.source_sha256 == hash)
                || marker.is_some_and(|marker| &marker.source_evidence_sha256 == hash));
        }
        return Ok(report);
    }
    if validate_only { return Ok(report); }
    // All semantic validation completes before creating anything. The original
    // backup remains immutable and contains every original table/outcome byte.
    let mut directory = std::fs::DirBuilder::new();
    #[cfg(unix)] {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(destination)?;
    std::fs::write(destination.join("crew-import-incomplete"), b"Do not open until import completes\n")?;
    let store = DocsStore::open_import_destination(destination)?;
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
        for entry in staged.prepare("SELECT doc_id,container,record_id,value FROM retained_records ORDER BY rowid")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Vec<u8>>(3)?)))? {
            let (doc_id, container, record_id, value) = entry?;
            tx.execute("INSERT INTO legacy_crew_records(doc_id,container,record_id,value) VALUES (?1,?2,?3,?4)",
                rusqlite::params![doc_id, container, record_id, value])?;
        }
        tx.commit()?;
    }
    store.import_processed_commands(&claims)?;
    for entry in staged.prepare("SELECT doc_id,bytes FROM converted_snapshots ORDER BY doc_id")?
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))? {
        let (id, bytes) = entry?;
        store.save_snapshot(&id, &bytes)?;
    }
    store.save_snapshot(REGISTRY_DOC_ID, &registry.to_bytes()?)?;
    std::fs::write(destination.join("crew-import-source.json"), &manifest_bytes)?;
    std::fs::write(destination.join("crew-import-report.json"), serde_json::to_vec_pretty(&report)?)?;
    std::fs::remove_file(destination.join("crew-import-incomplete"))?;
    Ok(report)
}
