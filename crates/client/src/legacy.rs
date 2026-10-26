//! Offline iOS CometDocs hydration. Only a verified identity may select scoped
//! caches; originals and uncertain intents never become an execution outbox.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use loro::ToJson;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeron_doc::{RegistryDoc, SessionDoc, REGISTRY_DOC_ID};
use zeron_proto::{CollaborationPrincipal, CrewChatMetadata, SessionEnvironment};
use zeron_sync::DocsStore;
use crate::{ClientError, error::Result};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyImportReport {
    pub schema_version: u32,
    pub directory: String,
    pub source_digest: String,
    pub owner_subject: String,
    pub project_id: String,
    pub deployment_id: String,
    pub files: Vec<LegacyFile>,
    pub chat_ids: Vec<String>,
    pub native_session_ids: BTreeMap<String, String>,
    pub pending_files: usize,
    pub requires_reconciliation: bool,
    /// Exact retained records, including original command IDs and terminal results.
    /// These are intentionally not queued to the host or the chat outbox.
    pub retained_intents: BTreeMap<String, Value>,
    pub command_outcomes: BTreeMap<String, String>,
    pub missing_data: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyFile {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
    pub disposition: String,
}
fn storage(error: impl std::fmt::Display) -> ClientError { ClientError::Storage(error.to_string()) }
fn invalid(message: impl Into<String>) -> ClientError { ClientError::InvalidArgument(message.into()) }
fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path).map_err(storage)?;
    file.write_all(bytes).map_err(storage)?;
    file.sync_all().map_err(storage)
}

// AppConfig.documentCacheId: Swift uses UTF-8 byte counts, not character counts.
fn cache_id(edge: &str, principal: &CollaborationPrincipal, room: &str, deployment: &str) -> String {
    let parts = [edge, "scaffold", &principal.subject, &principal.project_id, room, deployment];
    let mut digest = Sha256::new();
    for part in parts { digest.update(format!("{}:", part.len())); digest.update(part.as_bytes()); }
    format!("{}scoped-{:x}", if room.starts_with("ws4/") { "workspace-" } else { "" }, digest.finalize())
}
fn cache_names(edge: &str, principal: &CollaborationPrincipal, room: &str, deployment: &str) -> BTreeSet<String> {
    let edge = edge.trim_end_matches('/');
    [edge.to_owned(), format!("{edge}/")].iter().map(|edge| cache_id(edge, principal, room, deployment)).collect()
}
fn load_snapshot(path: &Path) -> Result<loro::LoroDoc> {
    let doc = loro::LoroDoc::new();
    let status = doc.import(&fs::read(path).map_err(storage)?).map_err(storage)?;
    if status.pending.is_some_and(|pending| !pending.is_empty()) || doc.state_vv() != doc.oplog_vv() {
        return Err(invalid(format!("Incomplete legacy snapshot: {}", path.display())));
    }
    Ok(doc)
}
fn matching_file(files: &BTreeMap<String, Vec<u8>>, names: &BTreeSet<String>) -> Result<Option<String>> {
    let matches: Vec<_> = names.iter().map(|name| format!("{name}.loro")).filter(|name| files.contains_key(name)).collect();
    if matches.len() > 1 { return Err(invalid("Multiple scoped snapshots match the same identity; reconcile the copied backup first")); }
    Ok(matches.into_iter().next())
}

/// Decode Foundation Codable Data without adding a dependency. Reject truncated
/// or noncanonical bytes rather than silently changing a retained record.
fn data(value: &Value) -> Result<Vec<u8>> {
    let text = value.as_str().ok_or_else(|| invalid("Legacy Data is not base64 text"))?;
    if text.len() % 4 != 0 { return Err(invalid("Truncated legacy Data")); }
    let mut output = Vec::with_capacity(text.len() / 4 * 3);
    for (index, chunk) in text.as_bytes().chunks_exact(4).enumerate() {
        let mut bits = 0u32;
        let mut padding = 0;
        for &byte in chunk {
            let digit = match byte {
                b'A'..=b'Z' => byte - b'A', b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52, b'+' => 62, b'/' => 63,
                b'=' => { padding += 1; 0 }, _ => return Err(invalid("Invalid legacy Data")),
            };
            if padding != 0 && byte != b'=' { return Err(invalid("Invalid base64 padding")); }
            bits = (bits << 6) | u32::from(digit);
        }
        if padding > 2 || (padding != 0 && index + 1 != text.len() / 4)
            || (padding == 1 && bits & 0xff != 0) || (padding == 2 && bits & 0xffff != 0) {
            return Err(invalid("Noncanonical legacy Data"));
        }
        output.push((bits >> 16) as u8);
        if padding < 2 { output.push((bits >> 8) as u8); }
        if padding == 0 { output.push(bits as u8); }
    }
    Ok(output)
}
fn record_data(value: Option<&Value>) -> Result<Option<Value>> {
    value.filter(|value| !value.is_null()).map(|value| serde_json::from_slice(&data(value)?).map_err(storage)).transpose()
}
fn scope_matches(environment: &SessionEnvironment, id: &str, principal: &CollaborationPrincipal) -> bool {
    environment.owner_principal == principal.subject && environment.scope.project_id == principal.project_id
        && environment.scope.deployment_id == principal.deployment_id && environment.scope.session_id.as_deref() == Some(id)
}
fn metadata(row: &Value, refs: &serde_json::Map<String, Value>, id: &str, principal: &CollaborationPrincipal) -> Result<CrewChatMetadata> {
    let metadata = if let Some(value) = row.get("crew").filter(|value| !value.is_null()) {
        serde_json::from_value::<CrewChatMetadata>(value.clone()).map_err(storage)?
    } else {
        let mut environments = refs.values().filter(|row| row["chatId"].as_str() == Some(id)
            && row["userId"].as_str() == Some(principal.subject.as_str()))
            .filter_map(|row| row.get("environment").filter(|value| !value.is_null()));
        let environment = environments.next().ok_or_else(|| invalid(format!(
            "Session {id} lacks owner/project/deployment authority: restore its authenticated directory membership before import")))?;
        if environments.any(|other| other != environment) { return Err(invalid(format!("Conflicting membership routes for {id}"))); }
        let environment: SessionEnvironment = serde_json::from_value(environment.clone()).map_err(storage)?;
        if !scope_matches(&environment, id, principal) { return Err(invalid(format!("Foreign session scope cannot be relabeled: {id}"))); }
        CrewChatMetadata { owner_subject: environment.owner_principal.clone(), scope: environment.scope.clone(),
            session_id: id.into(), environment: Some(environment), agent_route: None, original_epoch_history: None }
    };
    metadata.validate().map_err(invalid)?;
    if metadata.owner_subject != principal.subject || metadata.scope.project_id != principal.project_id
        || metadata.scope.deployment_id != principal.deployment_id || metadata.session_id != id {
        return Err(invalid(format!("Foreign session scope cannot be relabeled: {id}")));
    }
    Ok(metadata)
}

/// Called only before constructing the client. Stage a complete new profile and
/// atomically publish it; a killed import leaves a copy, never a half-live writer.
/// A matching durable receipt permits reopening an already-used native profile.
pub fn hydrate_backup(source: &Path, profile: &Path, principal: &CollaborationPrincipal, edge: &str, device_id: &str) -> Result<LegacyImportReport> {
    let deployment = principal.deployment_id.as_deref().filter(|id| !id.is_empty())
        .ok_or_else(|| invalid("Verified deployment identity is required"))?;
    if principal.session_id.is_some() || principal.subject.is_empty() || principal.project_id.is_empty() || device_id.is_empty() {
        return Err(invalid("A verified project user and local device identity are required"));
    }
    if fs::symlink_metadata(source).map_err(storage)?.file_type().is_symlink() || !source.is_dir() {
        return Err(invalid("Select a copied CometDocs folder, not a symlink"));
    }
    let source = source.canonicalize().map_err(storage)?;
    let parent = profile.parent().ok_or_else(|| invalid("Profile has no parent"))?;
    fs::create_dir_all(parent).map_err(storage)?;
    let parent = parent.canonicalize().map_err(storage)?;
    if parent.starts_with(&source) { return Err(invalid("Destination must be outside the source backup")); }
    let mut files = BTreeMap::new();
    let mut total = 0u64;
    for entry in fs::read_dir(&source).map_err(storage)? {
        let entry = entry.map_err(storage)?;
        let kind = entry.file_type().map_err(storage)?;
        let size = entry.metadata().map_err(storage)?.len();
        if !kind.is_file() || size > 64 * 1024 * 1024 { return Err(invalid("CometDocs must contain only regular files of at most 64 MiB")); }
        total = total.checked_add(size).ok_or_else(|| invalid("Backup size overflow"))?;
        if total > 512 * 1024 * 1024 || files.len() >= 4096 { return Err(invalid("Backup exceeds 512 MiB or 4096 files")); }
        let name = entry.file_name().into_string().map_err(|_| invalid("Invalid backup filename"))?;
        // CometDocs is documents only. Never copy a credential into the profile.
        if !(name.ends_with(".loro") || name.ends_with(".loro.recovery") || name.ends_with(".intents")
            || name.ends_with(".intents.recovery") || name.ends_with(".outcome")) {
            return Err(invalid(format!("Not a CometDocs document/journal: {name}")));
        }
        let bytes = fs::read(entry.path()).map_err(storage)?;
        if bytes.len() as u64 != size { return Err(invalid("Source backup changed while copying; stop its writer")); }
        files.insert(name, bytes);
    }
    if files.is_empty() {
        return Ok(LegacyImportReport { schema_version: 2, directory: profile.to_string_lossy().into_owned(),
            source_digest: String::new(), owner_subject: principal.subject.clone(), project_id: principal.project_id.clone(),
            deployment_id: deployment.into(), files: Vec::new(), chat_ids: Vec::new(), native_session_ids: BTreeMap::new(),
            pending_files: 0, requires_reconciliation: false, retained_intents: BTreeMap::new(),
            command_outcomes: BTreeMap::new(), missing_data: Vec::new() });
    }
    let inventory: BTreeMap<_, _> = files.iter().map(|(name, bytes)| (name.clone(), format!("{:x}", Sha256::digest(bytes)))).collect();
    let source_digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&inventory).map_err(storage)?));
    if profile.exists() {
        let receipt = fs::read(profile.join("legacy-ios-import.json")).map_err(|_| invalid("Destination already exists without a completed mobile import receipt; refusing to overwrite live state"))?;
        let report: LegacyImportReport = serde_json::from_slice(&receipt).map_err(storage)?;
        if report.schema_version != 2 || report.source_digest != source_digest || report.owner_subject != principal.subject
            || report.project_id != principal.project_id || report.deployment_id != deployment {
            return Err(invalid("Existing mobile import receipt has a different backup or authenticated scope"));
        }
        return Ok(report);
    }
    let stage = parent.join(format!(".legacy-ios-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&stage).map_err(storage)?;
    fs::create_dir(stage.join("originals")).map_err(storage)?;
    for (name, bytes) in &files { save(&stage.join("originals").join(name), bytes)?; }
    fs::File::open(stage.join("originals")).and_then(|file| file.sync_all()).map_err(storage)?;
    save(&stage.join("source-inventory.json"), &serde_json::to_vec(&inventory).map_err(storage)?)?;
    // Parse only the frozen copies, never the still-user-owned source directory.
    let result = hydrate_copies(&stage, profile, &files, &inventory, source_digest, principal, edge, device_id);
    let report = result.map_err(|error| invalid(format!("{error}; byte-exact backup retained at {}", stage.display())))?;
    save(&stage.join("legacy-ios-import.json"), &serde_json::to_vec_pretty(&report).map_err(storage)?)?;
    sync_tree(&stage)?;
    // Another import/client must not have appeared while validation was running.
    if profile.exists() { return Err(invalid("Destination appeared during import; staged originals retained")); }
    fs::rename(&stage, profile).map_err(storage)?;
    fs::File::open(parent).and_then(|file| file.sync_all()).map_err(storage)?;
    Ok(report)
}
fn sync_tree(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root).map_err(storage)? {
        let entry = entry.map_err(storage)?;
        if entry.file_type().map_err(storage)?.is_dir() { sync_tree(&entry.path())?; }
        else { fs::File::open(entry.path()).and_then(|file| file.sync_all()).map_err(storage)?; }
    }
    fs::File::open(root).and_then(|file| file.sync_all()).map_err(storage)
}

fn hydrate_copies(stage: &Path, profile: &Path, files: &BTreeMap<String, Vec<u8>>, inventory: &BTreeMap<String, String>,
    source_digest: String, principal: &CollaborationPrincipal, edge: &str, device_id: &str) -> Result<LegacyImportReport> {
    let deployment = principal.deployment_id.as_deref().expect("validated deployment");
    let mut workspace_names = cache_names(edge, principal, &format!("ws4/{}", principal.project_id), "");
    // WorkspaceStore's one-time migration read the same hash without workspace-.
    workspace_names.extend(workspace_names.clone().into_iter().map(|name| name.trim_start_matches("workspace-").to_owned()));
    let workspace_file = matching_file(files, &workspace_names)?.ok_or_else(|| invalid(
        "Missing workspace snapshot bound to this verified owner, project and Scaffold edge origin"))?;
    let workspace_doc = load_snapshot(&stage.join("originals").join(&workspace_file))?;
    let workspace = std::sync::Arc::new(workspace_doc.get_deep_value().to_json_value());
    let mut chats = workspace.get("chats").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut spaces = workspace.get("spaces").and_then(Value::as_object).cloned().unwrap_or_default();
    let empty = serde_json::Map::new();
    let refs = workspace.get("sessionRefs").and_then(Value::as_object).unwrap_or(&empty);
    let mut report = LegacyImportReport { schema_version: 2, directory: profile.to_string_lossy().into_owned(), source_digest,
        owner_subject: principal.subject.clone(), project_id: principal.project_id.clone(), deployment_id: deployment.into(),
        files: files.iter().map(|(name, bytes)| LegacyFile { name: name.clone(), bytes: bytes.len() as u64,
            sha256: inventory[name].clone(), disposition: "preserved-unbound-not-published".into() }).collect(),
        chat_ids: Vec::new(), native_session_ids: BTreeMap::new(), pending_files: 0, requires_reconciliation: false,
        retained_intents: BTreeMap::new(), command_outcomes: BTreeMap::new(), missing_data: Vec::new() };
    for name in &workspace_names {
        let name = format!("{name}-session-preparations.intents");
        let Some(bytes) = files.get(&name) else { continue };
        let value: Value = serde_json::from_slice(bytes).map_err(storage)?;
        for prepared in value.as_array().ok_or_else(|| invalid("Invalid session preparation journal"))? {
            let mut chat = prepared["bootstrap"]["chat"].clone();
            let space = &prepared["bootstrap"]["space"];
            let id = chat["id"].as_str().ok_or_else(|| invalid("Prepared chat has no public ID"))?.to_owned();
            let space_id = space["id"].as_str().ok_or_else(|| invalid("Prepared chat has no space"))?;
            let authority = &prepared["authority"];
            let environment: SessionEnvironment = serde_json::from_value(authority["environment"].clone())
                .map_err(|_| invalid(format!("Prepared session {id} has no retained owner/deployment environment; authenticated host reconciliation is required")))?;
            if !scope_matches(&environment, &id, principal) || authority["actorSubject"].as_str() != Some(principal.subject.as_str())
                || chat["deviceId"] != space["deviceId"] || chat["spaceId"].as_str() != Some(space_id)
                || authority["controllerDeviceId"] != space["deviceId"]
                || chat["cwd"].as_str().is_none_or(str::is_empty) || chat["config"].is_null() {
                return Err(invalid(format!("Prepared session {id} has conflicting retained scope/host/worktree")));
            }
            let prepared_metadata = if chat.get("crew").is_some_and(|value| !value.is_null()) {
                let supplied = metadata(&chat, refs, &id, principal)?;
                if supplied.environment.as_ref().is_some_and(|supplied| supplied != &environment) {
                    return Err(invalid(format!("Prepared session {id} has conflicting supplied environment provenance")));
                }
                supplied
            } else {
                CrewChatMetadata { owner_subject: principal.subject.clone(), scope: environment.scope.clone(),
                    session_id: id.clone(), environment: Some(environment), agent_route: None, original_epoch_history: None }
            };
            if let Some(existing) = chats.get_mut(&id) {
                for field in ["id", "deviceId", "spaceId", "cwd", "config"] {
                    if existing[field] != chat[field] { return Err(invalid(format!("Prepared session {id} conflicts with cached {field}"))); }
                }
                if let Some(history) = &prepared_metadata.original_epoch_history {
                    let mut retained = metadata(existing, refs, &id, principal)?;
                    if retained.original_epoch_history.as_ref().is_some_and(|existing| existing != history) {
                        return Err(invalid(format!("Prepared session {id} has conflicting original epoch provenance")));
                    }
                    retained.original_epoch_history = Some(history.clone());
                    retained.validate().map_err(invalid)?;
                    existing["crew"] = serde_json::to_value(retained).map_err(storage)?;
                }
            } else {
                chat["crew"] = serde_json::to_value(prepared_metadata).map_err(storage)?;
                chats.insert(id, chat);
            }
            if let Some(existing) = spaces.get(space_id) {
                if existing["deviceId"] != space["deviceId"] || existing["path"] != space["path"] {
                    return Err(invalid("Prepared space conflicts with cached host/worktree"));
                }
            } else { spaces.insert(space_id.into(), space.clone()); }
        }
        report.pending_files += 1;
        report.retained_intents.insert(name, value);
    }
    let mut registry = RegistryDoc::new(device_id);
    let mut documents = BTreeMap::new();
    let mut bindings = BTreeMap::new();
    bindings.insert(workspace_file.trim_end_matches(".loro").to_owned(), (None, workspace.clone()));
    let mut used_spaces = BTreeSet::new();
    let mut used_devices = BTreeSet::new();
    let mut claims = BTreeSet::new();
    for (id, row) in &chats {
        let metadata = match metadata(row, refs, id, principal) {
            Ok(metadata) => metadata,
            Err(error) => {
                // ws4 is project-wide; it contains other users' host rows too.
                // Never turn their presence in our cache into ownership.
                report.missing_data.push(error.to_string());
                continue;
            }
        };
        if uuid::Uuid::parse_str(id).is_err() || row["id"].as_str() != Some(id.as_str()) {
            return Err(invalid(format!("Session identity is not an exact public UUID: {id}; reconcile legacy aliases first")));
        }
        let mut names = cache_names(edge, principal, id, deployment);
        // Old desktop sessions omitted deployment in the filename. Only the
        // separately checked owned membership above can bind that cache.
        names.extend(cache_names(edge, principal, id, ""));
        let filename = matching_file(files, &names)?;
        let mut fields: BTreeMap<String, Value> = serde_json::from_value(row.clone()).map_err(storage)?;
        if let Some(config) = fields.get_mut("config").filter(|value| !value.is_null()) {
            // Legacy iOS ChatConfig.sandbox was optional; SessionStore used
            // `config?.sandbox ?? "workspace-write"` when constructing a run.
            if let Some(object) = config.as_object_mut()
                && object.get("sandbox").is_none_or(Value::is_null)
            {
                object.insert("sandbox".into(), Value::from("workspace-write"));
            }
            let _: zeron_proto::ChatConfig = serde_json::from_value(config.clone()).map_err(storage)?;
        }
        if let Some(space) = row["spaceId"].as_str() { used_spaces.insert(space.to_owned()); }
        if let Some(device) = row["deviceId"].as_str() { used_devices.insert(device.to_owned()); }
        if let Some(native) = row["harnessSessionId"].as_str().filter(|id| !id.is_empty()) {
            report.native_session_ids.insert(id.clone(), native.into());
            report.missing_data.push(format!("Session {id}: native journal {native} remains on its owning host; CometDocs contains transcript/cache, not the host execution journal"));
        }
        fields.insert("crew".into(), serde_json::to_value(metadata).map_err(storage)?);
        fields.insert("roomGen".into(), Value::from(2));
        registry.import_crew_row("chats", id, fields).map_err(storage)?;
        report.chat_ids.push(id.clone());
        if let Some(filename) = filename {
            let raw = load_snapshot(&stage.join("originals").join(&filename))?;
            let doc = SessionDoc::from_doc(raw);
            if doc.chat_id().as_deref() != Some(id.as_str()) { return Err(invalid(format!("Snapshot chat identity mismatch: {id}"))); }
            let root = std::sync::Arc::new(doc.doc().get_deep_value().to_json_value());
            if !matches!(root["meta"]["schemaVersion"].as_u64(), Some(1 | 2)) { return Err(invalid(format!("Unsupported session schema: {id}"))); }
            for value in root.get("agentSessions").and_then(Value::as_object).into_iter().flat_map(|rows| rows.values())
                .chain(root.get("publications").and_then(Value::as_array).into_iter().flatten().filter_map(|row| row.get("record"))) {
                let publication: zeron_proto::PublicationRecord = serde_json::from_value(value.clone()).map_err(storage)?;
                zeron_doc::validate_publication(&publication).map_err(storage)?;
                match publication.value {
                    zeron_proto::PublicationValue::AgentSession(session) => {
                        if session.owner_subject != principal.subject || session.chat_id != *id || session.session_id != *id {
                            return Err(invalid(format!("Foreign agent membership requires authenticated reattachment: {id}")));
                        }
                    }
                    zeron_proto::PublicationValue::Annotation(annotation) => {
                        doc.doc().get_map("crewAnnotations").insert(&annotation.id,
                            loro::LoroValue::from(serde_json::to_value(&annotation).map_err(storage)?)).map_err(storage)?;
                    }
                    _ => {}
                }
            }
            for command in root.get("commands").and_then(Value::as_array).into_iter().flatten() {
                let command_id = command["id"].as_str().ok_or_else(|| invalid("Command has no ID"))?;
                if !claims.insert(command_id.to_owned()) { return Err(invalid(format!("Duplicate command identity: {command_id}"))); }
                let status = command["status"].as_str().ok_or_else(|| invalid("Command has no outcome"))?;
                let _: zeron_doc::SessionCommandStatus = serde_json::from_value(command["status"].clone()).map_err(storage)?;
                report.command_outcomes.insert(command_id.into(), if status == "pending" { "unknown" } else { status }.into());
                if status == "pending" { report.requires_reconciliation = true; }
            }
            // Retain a lossless original, but project Crew-only transcript parts
            // with the same converter used by desktop migration.
            zeron_sync::crew_import::convert_legacy_parts(&doc, id).map_err(storage)?;
            let message_count = root.get("messages").and_then(Value::as_array).map_or(0, Vec::len);
            if doc.read_entries().map_err(storage)?.len() != message_count { return Err(invalid(format!("Transcript entry cannot be represented: {id}"))); }
            for message in root.get("messages").and_then(Value::as_array).into_iter().flatten() {
                for attachment in message.get("parts").and_then(Value::as_array).into_iter().flatten()
                    .filter(|part| part["kind"].as_str() == Some("image")) {
                    if let Some(path) = attachment.get("path").and_then(Value::as_str) {
                        report.missing_data.push(format!("Session {id}: transcript attachment {path} is a host reference; CometDocs has no corresponding file bytes"));
                    }
                }
            }
            doc.doc().get_map("meta").insert("epoch", 2i64).map_err(storage)?;
            doc.doc().commit();
            documents.insert(id.clone(), doc.export_snapshot().map_err(storage)?);
            for name in names { bindings.insert(name, (Some(id.clone()), root.clone())); }
        } else {
            report.missing_data.push(format!("Session {id}: scoped .loro snapshot absent (legacy LRU may have pruned it); authenticated host backfill is required"));
            for name in names { bindings.insert(name, (Some(id.clone()), std::sync::Arc::new(Value::Null))); }
        }
    }
    if !chats.is_empty() && report.chat_ids.is_empty() {
        return Err(invalid(format!("No legacy sessions have verified native hydration authority: {}", report.missing_data.join("; "))));
    }
    for row in refs.values() {
        if row["userId"].as_str() == Some(principal.subject.as_str()) {
            if let Some(id) = row["chatId"].as_str().filter(|id| !chats.contains_key(*id)) {
                return Err(invalid(format!("Membership {id} has no cached host chat row/config/worktree; authenticate and backfill the directory before import")));
            }
        }
    }
    for (kind, wanted) in [("spaces", used_spaces), ("devices", used_devices)] {
        for id in wanted {
            let row = if kind == "spaces" { spaces.get(&id) }
                else { workspace.get(kind).and_then(|rows| rows.get(&id)) }
                .ok_or_else(|| invalid(format!("Missing {kind} reference: {id}")))?;
            registry.import_crew_row(kind, &id, serde_json::from_value(row.clone()).map_err(storage)?).map_err(storage)?;
        }
    }
    if registry.read_chats().map_err(storage)?.len() != report.chat_ids.len() { return Err(invalid("Legacy registry row is not representable by the native schema")); }
    for file in &mut report.files {
        if file.name.ends_with(".loro.recovery") || file.name.ends_with(".intents.recovery") {
            file.disposition = "preserved-recovery-original-never-replayed".into();
            continue;
        }
        if report.retained_intents.contains_key(&file.name) {
            file.disposition = "hydrated-preparation-retained-never-replayed".into();
            continue;
        }
        let (stem, outcome_id) = if let Some(stem) = file.name.strip_suffix(".intents") { (stem, None) }
            else if let Some((stem, suffix)) = file.name.split_once(".intents.") {
                let id = suffix.strip_suffix(".outcome").ok_or_else(|| invalid("Unknown intent sidecar"))?;
                if uuid::Uuid::parse_str(id).is_err() { return Err(invalid("Invalid outcome filename ID")); }
                (stem, Some(id))
            } else {
                if file.name.strip_suffix(".loro").is_some_and(|stem| bindings.contains_key(stem)) {
                    file.disposition = "hydrated-native".into();
                } else {
                    report.missing_data.push(format!("{}: scoped cache does not match any verified owned session; retained without relabeling", file.name));
                }
                continue;
            };
        let Some((session, root)) = bindings.get(stem) else {
            report.missing_data.push(format!("{}: no exact verified session binding; preserved without adopting its scope", file.name));
            continue;
        };
        let value: Value = serde_json::from_slice(&files[&file.name]).map_err(storage)?;
        let records: Vec<&Value> = if outcome_id.is_some() { vec![&value] }
            else { value.as_array().ok_or_else(|| invalid("Intent journal is not an array"))?.iter().collect() };
        for record in records {
            let key = record.get(if session.is_some() { "messageId" } else { "id" }).and_then(Value::as_str)
                .ok_or_else(|| invalid(format!("Missing retained intent ID: {}", file.name)))?;
            if outcome_id.is_some_and(|id| id != key) { return Err(invalid("Outcome filename and retained ID disagree")); }
            if session.is_none() {
                if uuid::Uuid::parse_str(key).is_err() { return Err(invalid("Invalid workspace intent identity")); }
                // Workspace edits may already be reflected in the cached state.
                // Never silently apply a pending before/after over unrelated state.
                let container = record["root"].as_str().ok_or_else(|| invalid("Record intent has no root"))?;
                let row = record["key"].as_str().ok_or_else(|| invalid("Record intent has no key"))?;
                if outcome_id.is_none() && !files.contains_key(&format!("{stem}.intents.{key}.outcome"))
                    && root.get(container).and_then(|rows| rows.get(row)).filter(|v| !v.is_null()) != record_data(record.get("after"))?.as_ref() {
                    return Err(invalid(format!("Unreconciled workspace intent {key} ({container}/{row}); snapshot does not contain its retained final value")));
                }
            } else {
                if key != "composer" && uuid::Uuid::parse_str(key).is_err() { return Err(invalid("Invalid retained command identity")); }
                if let Some(scope) = record.get("admission").and_then(|admission| admission.get("scope")).filter(|v| !v.is_null()) {
                    if scope["projectId"].as_str() != Some(principal.project_id.as_str()) || scope["deploymentId"].as_str() != Some(deployment)
                        || scope["sessionId"].as_str() != session.as_deref() { return Err(invalid("Retained command belongs to another scope")); }
                }
                if key != "composer" {
                    claims.insert(key.into());
                    let status = if record["terminal"].as_bool() == Some(true) {
                        if record.get("failure").is_some_and(|v| !v.is_null()) { "rejected" } else { "applied" }
                    } else { "unknown" };
                    let previous = report.command_outcomes.entry(key.into()).or_insert_with(|| status.into());
                    if status != "unknown" {
                        if previous != "unknown" && previous != status {
                            return Err(invalid(format!("Conflicting retained outcome: {key}")));
                        }
                        *previous = status.into();
                    }
                }
                for image in record.get("images").and_then(Value::as_array).into_iter().flatten() {
                    let id = image["id"].as_str().ok_or_else(|| invalid("Retained attachment has no ID"))?;
                    if uuid::Uuid::parse_str(id).is_err() { return Err(invalid("Invalid attachment identity")); }
                    let bytes = data(&image["bytes"])?;
                    let directory = stage.join("legacy-attachments");
                    fs::create_dir_all(&directory).map_err(storage)?;
                    let path = directory.join(id);
                    if path.exists() {
                        if fs::read(&path).map_err(storage)? != bytes { return Err(invalid("Conflicting retained attachment bytes")); }
                    } else { save(&path, &bytes)?; }
                }
            }
        }
        report.pending_files += 1;
        report.retained_intents.insert(file.name.clone(), value);
        file.disposition = "retained-native-reconciliation-never-replayed".into();
    }
    report.requires_reconciliation |= report.pending_files != 0 || !report.missing_data.is_empty();
    let store = DocsStore::open(stage).map_err(storage)?;
    let claims: Vec<_> = claims.into_iter().map(|id| (id, 0, None)).collect();
    store.import_processed_commands(&claims).map_err(storage)?;
    for (id, bytes) in documents {
        store.save_snapshot_with_cursor(&id, &bytes, 0, 2).map_err(storage)?;
        store.initialize_chat_outbox(&id, &[]).map_err(storage)?;
    }
    store.save_snapshot(REGISTRY_DOC_ID, &registry.to_bytes().map_err(storage)?).map_err(storage)?;
    drop(store);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_hydration_preserves_identity_and_unknown_commands_without_replay() {
        for sandbox in [None, Some(Value::Null)] {
        let source = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("profile");
        let principal = CollaborationPrincipal { subject: "owner".into(), email: None, project_id: "project".into(),
            deployment_id: Some("deployment".into()), session_id: None, capabilities: vec![], unknown: BTreeMap::new() };
        let id = "8b3ca06e-1fe0-4b72-bbe3-7bc7ec760baf";
        let command = "8b3ca06e-1fe0-4b72-bbe3-7bc7ec760bae";
        let edge = "https://edge.example.com";
        assert!(hydrate_backup(source.path(), &target, &principal, edge, "phone").unwrap().files.is_empty());
        assert!(!target.exists());
        let workspace = loro::LoroDoc::new();
        let history = zeron_proto::OriginalEpochHistory { sandbox_id: "retained-sandbox".into(), lifecycle_epoch: 7,
            coordinator_lifecycle_epoch: 7,
            source_device_id: "comet-scaffold-retained-sandbox".into(), original_status: "stopped".into(), native_session_id: Some("exact-native".into()),
            source_evidence_sha256: "a".repeat(64) };
        let metadata = CrewChatMetadata { owner_subject: "owner".into(), scope: zeron_proto::CollaborationScope {
            project_id: "project".into(), deployment_id: Some("deployment".into()), session_id: Some(id.into()), unknown: Default::default() },
            session_id: id.into(), environment: None, agent_route: None, original_epoch_history: Some(history.clone()) };
        let mut original_config = serde_json::json!({"harness":"omp","model":"exact-model"});
        if let Some(sandbox) = sandbox { original_config["sandbox"] = sandbox; }
        workspace.get_map("chats").insert(id, loro::LoroValue::from(serde_json::json!({
            "id":id,"deviceId":"host","title":"Retained title","createdAt":1,"cwd":"/exact/worktree",
            "harnessSessionId":"exact-native","crew":metadata,
            "config":original_config
        }))).unwrap();
        workspace.get_map("devices").insert("host", loro::LoroValue::from(serde_json::json!({"id":"host","name":"Host","kind":"desktop"}))).unwrap();
        let workspace_name = format!("{}.loro", cache_id(edge, &principal, "ws4/project", ""));
        fs::write(source.path().join(&workspace_name), workspace.export(loro::ExportMode::Snapshot).unwrap()).unwrap();
        let session = SessionDoc::init(id).unwrap();
        session.doc().get_map("retainedMetadata").insert("nativeHistoryId", "exact-native").unwrap();
        let stem = cache_id(edge, &principal, id, "deployment");
        let snapshot = session.export_snapshot().unwrap();
        fs::write(source.path().join(format!("{stem}.loro")), &snapshot).unwrap();
        fs::write(source.path().join(format!("{stem}.intents")), serde_json::to_vec(&serde_json::json!([
            {"messageId":command,"prompt":"Retain me","terminal":false,"images":[
                {"id":command,"filename":"original.png","bytes":"aGVsbG8="}
            ]}
        ])).unwrap()).unwrap();
        let report = hydrate_backup(source.path(), &target, &principal, edge, "phone").unwrap();
        assert_eq!(report.chat_ids, [id]);
        assert_eq!(report.native_session_ids[id], "exact-native");
        assert_eq!(report.command_outcomes[command], "unknown");
        let store = DocsStore::open(&target).unwrap();
        let registry = RegistryDoc::from_bytes(&store.load_snapshot(REGISTRY_DOC_ID).unwrap().unwrap(), "phone").unwrap();
        let chat = registry.read_chats().unwrap().pop().unwrap();
        assert_eq!(chat.harness_session_id.as_deref(), Some("exact-native"));
        assert_eq!(chat.cwd.as_deref(), Some("/exact/worktree"));
        let config = chat.config.unwrap();
        assert_eq!(config.model.as_deref(), Some("exact-model"));
        assert_eq!(config.sandbox, zeron_proto::SandboxLevel::WorkspaceWrite);
        assert_eq!(chat.crew.unwrap().original_epoch_history, Some(history));
        assert_eq!(fs::read(target.join("legacy-attachments").join(command)).unwrap(), b"hello");
        let local = crate::live::room::load_local(&store, id);
        assert!(local.had_content);
        assert_eq!(local.doc.chat_id().as_deref(), Some(id));
        assert_eq!(local.doc.doc().get_deep_value().to_json_value()["retainedMetadata"]["nativeHistoryId"], "exact-native");
        assert!(store.pending_chat_updates(id).unwrap().is_empty());
        assert!(store.chat_outbox_initialized(id).unwrap());
        drop(store);
        assert_eq!(fs::read(source.path().join(format!("{stem}.loro"))).unwrap(), snapshot);
        assert_eq!(hydrate_backup(source.path(), &target, &principal, edge, "phone").unwrap().source_digest, report.source_digest);
        let mut foreign = principal.clone(); foreign.deployment_id = Some("other".into());
        assert!(hydrate_backup(source.path(), &target, &foreign, edge, "phone").is_err());
        assert_eq!(data(&Value::from("aGVsbG8=")).unwrap(), b"hello");
        assert!(data(&Value::from("aGVsbG9=")).is_err());
        }
    }
}
