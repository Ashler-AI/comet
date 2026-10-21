use std::collections::{BTreeMap, BTreeSet};

use loro::{LoroDoc, LoroList, LoroMap, LoroText, LoroValue, ToJson};
use serde_json::{Map, Value};
use zeron_proto::{CollaborationPrincipal, PublicationRecord, PublicationValue};

use crate::crew_import::{CrewBackupManifest, CrewImportError};

type Result<T, E = CrewImportError> = std::result::Result<T, E>;

fn unsafe_data(message: impl Into<String>) -> CrewImportError {
    CrewImportError::Unsafe(message.into())
}

fn change_meta(doc: &LoroDoc, id: loro::ID) -> Result<Option<loro::ChangeMeta>> {
    Ok(doc.with_oplog(|oplog| oplog.change_store().get_change_meta(id))?)
}

/// Compare causal state without caching a version vector for every ancestor.
/// Keep the native trimmed-history semantics for shallow snapshots.
pub(crate) fn state_matches_oplog(doc: &LoroDoc, version: &loro::VersionVector) -> Result<bool> {
    frontiers_match_oplog(doc, &doc.state_frontiers(), version)
}

fn frontiers_match_oplog(doc: &LoroDoc, frontiers: &loro::Frontiers, version: &loro::VersionVector) -> Result<bool> {
    let baseline = doc.shallow_since_vv();
    let root_deps = if baseline.is_empty() {
        None
    } else {
        if baseline.iter().any(|(peer, end)| *end < 0 || *end > version.get(peer).copied().unwrap_or(0)) {
            return Err(unsafe_data("shallow baseline lies outside the oplog"));
        }
        let root = doc.shallow_since_frontiers().as_single()
            .ok_or_else(|| unsafe_data("invalid shallow causal root"))?;
        let change = change_meta(doc, root)?.ok_or_else(|| unsafe_data("missing shallow causal root"))?;
        if change.id != root || baseline.get(&root.peer).copied().unwrap_or(0) != root.counter
            || change.len == 0 || change.deps.iter().any(|id| id.counter < 0 || baseline.get(&id.peer).copied().unwrap_or(0) <= id.counter) {
            return Err(unsafe_data("invalid shallow causal root span"));
        }
        Some(change.deps)
    };
    // Pinned Loro treats the dependencies immediately before the retained root
    // as the trimmed baseline, not as individually resolvable history entries.
    if root_deps.as_ref() == Some(frontiers) {
        return Ok(baseline.to_vv() == *version);
    }
    let mut reachable = loro::VersionVector::default();
    let mut visited = BTreeSet::new();
    let mut pending: Vec<_> = frontiers.iter().map(|id| (id, None::<u32>)).collect();
    while let Some((id, parent_lamport)) = pending.pop() {
        if !version.includes_id(id) {
            return Err(unsafe_data("causal frontier or dependency lies outside the oplog"));
        }
        if baseline.get(&id.peer).copied().unwrap_or(0) > id.counter {
            if parent_lamport.is_none() {
                return Err(unsafe_data("state frontier precedes retained history"));
            }
            reachable.extend_to_include_vv(baseline.iter());
            continue;
        }
        let change = change_meta(doc, id)?.ok_or_else(|| unsafe_data("missing causal dependency"))?;
        let end = i32::try_from(change.len).ok().and_then(|len| change.id.counter.checked_add(len))
            .filter(|end| change.id.counter >= 0 && *end > change.id.counter
                && *end <= version.get(&id.peer).copied().unwrap_or(0))
            .ok_or_else(|| unsafe_data("invalid causal change span"))?;
        if change.id.peer != id.peer || id.counter < change.id.counter || id.counter >= end {
            return Err(unsafe_data("causal identity does not belong to its change"));
        }
        let lamport = change.lamport.checked_add((id.counter - change.id.counter) as u32)
            .ok_or_else(|| unsafe_data("causal Lamport overflow"))?;
        if parent_lamport.is_some_and(|parent| lamport >= parent) {
            return Err(unsafe_data("causal dependency does not precede its change"));
        }
        // A frontier can stop inside a change. Include only its requested prefix,
        // even when another path has already visited this change's dependencies.
        reachable.extend_to_include_last_id(id);
        if visited.insert((change.id.peer, change.id.counter)) {
            if root_deps.as_ref() == Some(&change.deps) {
                reachable.extend_to_include_vv(baseline.iter());
            } else {
                pending.extend(change.deps.iter().map(|dep| (dep, Some(change.lamport))));
            }
        }
    }
    Ok(reachable == *version)
}

// Same grammar as legacy workspace::execution_chat_id, including repeated self aliases.
pub(crate) fn canonical_execution_id(key: &str) -> Option<&str> {
    let (id, suffix) = key.split_once("::session::")?;
    (uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
        && suffix.split("::session::").all(|part| part == id))
    .then_some(id)
}

#[cfg(test)]
/// Classify borrowed source documents without changing their snapshots or history.
pub fn inert_bootstrap_snapshots(
    db: &rusqlite::Connection,
    documents: &BTreeMap<String, LoroDoc>,
    associated_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>> {
    let mut possible = inert_snapshot_ids(db)?;
    possible.extend(documents.keys().cloned());
    classify_inert_snapshots(db, documents.iter().map(|(id, doc)| Ok((id.clone(), doc))), possible, associated_ids)
}

/// Read and validate every current snapshot, retaining only one decoded document at a time.
pub(crate) fn inert_bootstrap_snapshots_from_store(
    db: &rusqlite::Connection,
    associated_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>> {
    let possible = inert_snapshot_ids(db)?;
    let mut statement = db.prepare("SELECT doc_id,bytes FROM snapshots WHERE substr(doc_id,1,length(?1))!=?1")?;
    let rows = statement.query_map(["recovery-original/"], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let documents = rows.map(|row| {
        let (id, bytes) = row?;
        let doc = LoroDoc::new();
        let status = doc.import(&bytes).map_err(|error| zeron_doc::DocError::Schema(error.to_string()))?;
        if status.pending.is_some() || doc.is_detached() || !state_matches_oplog(&doc, &doc.oplog_vv())? {
            return Err(unsafe_data(format!("snapshot has unresolved causal dependencies: {id}")));
        }
        Ok((id, doc))
    });
    classify_inert_snapshots(db, documents, possible, associated_ids)
}

fn inert_snapshot_ids(db: &rusqlite::Connection) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    let mut statement = db.prepare("SELECT doc_id FROM snapshots")?;
    for id in statement.query_map([], |row| row.get::<_, String>(0))? {
        if !ids.insert(id?) { return Err(unsafe_data("duplicate source snapshot identity")); }
    }
    Ok(ids)
}

struct InertReferences {
    possible: BTreeSet<String>,
    referenced: BTreeSet<String>,
}

impl InertReferences {
    fn insert(&mut self, id: &str) {
        if self.possible.contains(id) { self.referenced.insert(id.to_owned()); }
    }

    fn value(&mut self, value: &Value, parent: &str, physical_source: bool) {
        let physical_environment = parent == "environment"
            && <zeron_proto::SessionEnvironment as serde::Deserialize>::deserialize(value)
                .is_ok_and(|environment| matches!(environment.source, zeron_proto::SessionEnvironmentSource::Scaffold { .. }));
        match value {
            Value::String(id) => self.insert(id),
            Value::Array(values) => for value in values { self.value(value, parent, false); },
            Value::Object(fields) => for (key, value) in fields {
                if physical_source && key == "sandbox_id" { continue; }
                self.insert(key);
                self.value(value, key, physical_environment && key == "source");
            },
            _ => {}
        }
    }

    fn retained(&mut self, doc: &LoroDoc, id: &str) -> Result<bool> {
        if doc.is_shallow() || doc.is_detached() || !state_matches_oplog(doc, &doc.oplog_vv())? { return Ok(false); }
        let metadata = loro::ContainerID::new_root("meta", loro::ContainerType::Map);
        for (peer, end) in doc.oplog_vv().iter() {
            doc.with_oplog(|oplog| {
                oplog.change_store().try_visit_json_ops::<CrewImportError>(
                    loro::IdSpan::new(*peer, 0, *end),
                    |_, op| {
                        match op.content {
                            loro::JsonOpContent::Map(loro::JsonMapOp::Insert { key, value }) => {
                                let value = value.to_json_value();
                                if op.container == metadata && key == "chatId" && value.as_str() == Some(id) { return Ok(()); }
                                self.insert(&key);
                                self.value(&value, &key, false);
                            }
                            loro::JsonOpContent::Map(loro::JsonMapOp::Delete { key }) => self.insert(&key),
                            content => self.value(&serde_json::to_value(content)?, "", false),
                        }
                        Ok(())
                    },
                )
            })?;
        }
        Ok(true)
    }

    fn root(&mut self, root: &Value, id: &str) -> Result<()> {
        for (key, value) in root.as_object().ok_or_else(|| unsafe_data("invalid source document root"))? {
            self.insert(key);
            if key == "meta" {
                for (field, value) in value.as_object().ok_or_else(|| unsafe_data("invalid source document metadata"))? {
                    if field == "chatId" && value.as_str() == Some(id) { continue; }
                    self.insert(field);
                    self.value(value, field, false);
                }
            } else { self.value(value, key, false); }
        }
        Ok(())
    }
}

fn classify_inert_snapshots<D: std::borrow::Borrow<LoroDoc>>(
    db: &rusqlite::Connection,
    documents: impl IntoIterator<Item = Result<(String, D)>>,
    possible: BTreeSet<String>,
    associated_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>> {
    use sha2::{Digest, Sha256};
    let mut references = InertReferences { possible, referenced: BTreeSet::new() };
    for id in associated_ids { references.insert(id); }
    let mut eligible = BTreeMap::new();
    let mut complete = true;
    for document in documents {
        let (id, doc) = document?;
        let doc = doc.borrow();
        // Keep decoding every later store row even when absence of references cannot be proved.
        if !complete { continue; }
        if !references.retained(doc, &id)? {
            complete = false;
            eligible.clear();
            continue;
        }
        let root = doc.get_deep_value().to_json_value();
        references.root(&root, &id)?;
        if pristine_bootstrap(doc, &id, &root)? {
            let bytes: Vec<u8> = db.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [&id], |row| row.get(0))?;
            eligible.insert(id, format!("{:x}", Sha256::digest(&bytes)));
        }
    }
    let mut recovery =
        db.prepare("SELECT doc_id,bytes FROM snapshots WHERE substr(doc_id,1,length(?1))=?1")?;
    let mut rows = recovery.query(["recovery-original/"])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        if let Some(original) = id.strip_prefix("recovery-original/") {
            references.insert(original);
            for part in original.split('/') { references.insert(part); }
        }
        let bytes: Vec<u8> = row.get(1)?;
        let doc = LoroDoc::new();
        let Ok(status) = doc.import(&bytes) else {
            complete = false;
            continue;
        };
        if status.pending.is_some() || !references.retained(&doc, &id)? {
            complete = false;
            continue;
        }
        references.value(&doc.get_deep_value().to_json_value(), "", false);
    }
    for (table, identity, payload) in [
        ("document_scopes", "doc_id", Some("scope")),
        ("semantic_intents", "doc_id", Some("value")),
        ("crew_directory_jobs", "session_id", None),
        ("pending_nudges", "chat_id", None),
        ("session_startups", "chat_id", None),
    ] {
        if !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )? {
            continue;
        }
        // Import accepts an empty startup sidecar without requiring its legacy
        // schema. It cannot associate a snapshot; populated tables still must
        // expose their identities for backup and desktop-source classification.
        if table == "session_startups"
            && !db.query_row("SELECT EXISTS(SELECT 1 FROM session_startups)", [], |row| {
                row.get::<_, bool>(0)
            })?
        {
            continue;
        }
        // Fixed schema identifiers only. Missing/wrong columns or payload types fail closed.
        let sql = format!(
            "SELECT {identity}{} FROM {table}",
            payload.map_or(String::new(), |field| format!(",{field}"))
        );
        let mut statement = db.prepare(&sql)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            references.insert(&row.get::<_, String>(0)?);
            if payload.is_some() {
                let bytes = match row.get_ref(1)? {
                    rusqlite::types::ValueRef::Text(bytes)
                    | rusqlite::types::ValueRef::Blob(bytes) => bytes,
                    _ => return Err(unsafe_data("invalid bootstrap association payload type")),
                };
                let value: Value = serde_json::from_slice(bytes)?;
                if !value.is_object() {
                    return Err(unsafe_data("invalid bootstrap association payload"));
                }
                references.value(&value, "", false);
            }
        }
    }
    if !complete { return Ok(BTreeMap::new()); }
    eligible.retain(|id, _| !references.referenced.contains(id));
    Ok(eligible)
}

fn pristine_bootstrap(doc: &LoroDoc, id: &str, root: &Value) -> Result<bool> {
        if id == "workspace3" || id.starts_with("recovery-original/")
            || doc.is_shallow() || doc.is_detached() || !state_matches_oplog(doc, &doc.oplog_vv())? { return Ok(false); }
        let root = root.as_object().ok_or_else(|| unsafe_data("invalid bootstrap root"))?;
        if root.iter().any(|(key, value)| match key.as_str() {
            "meta" => false,
            "messages" | "commands" => !value.as_array().is_some_and(Vec::is_empty),
            _ => true,
        }) {
            return Ok(false);
        }
        let Some(meta) = root.get("meta").and_then(Value::as_object) else {
            return Ok(false);
        };
        if meta.get("chatId").and_then(Value::as_str) != Some(id)
            || meta.get("schemaVersion").and_then(Value::as_u64) != Some(2)
            || meta.iter().any(|(key, value)| match key.as_str() {
                "chatId" | "schemaVersion" => false,
                "directoryCompletedTurn" => value.as_str() != Some(""),
                _ => true,
            })
        {
            return Ok(false);
        }
        let version = doc.oplog_vv();
        if version.iter().map(|(_, end)| i64::from(*end)).sum::<i64>() != meta.len() as i64 {
            return Ok(false);
        }
        let mut inserted = BTreeSet::new();
        let mut only_initial_metadata = true;
        let metadata = loro::ContainerID::new_root("meta", loro::ContainerType::Map);
        for (peer, end) in version.iter() {
            doc.with_oplog(|oplog| {
                oplog.change_store().try_visit_json_ops::<CrewImportError>(
                    loro::IdSpan::new(*peer, 0, *end),
                    |_, op| {
                        only_initial_metadata &= op.container == metadata && match op.content {
                            loro::JsonOpContent::Map(loro::JsonMapOp::Insert { key, value }) =>
                                meta.get(&key) == Some(&value.to_json_value()) && inserted.insert(key),
                            _ => false,
                        };
                        Ok(())
                    },
                )
            })?;
        }
        Ok(only_initial_metadata && inserted.len() == meta.len())
}

#[cfg(test)]
/// Reconcile offline semantic records, never CRDT-import two independently initialized
/// rooms. Their map/list operations have unrelated identity and could hide records.
/// Originals remain in the inventoried SQLite backup. No command is executed here.
/// Validate the entire plan before replacing any caller-owned document.
pub(crate) fn reconcile_execution_snapshots(
    documents: &mut BTreeMap<String, LoroDoc>,
    manifest: &CrewBackupManifest,
    principal: &CollaborationPrincipal,
) -> Result<BTreeMap<String, String>> {
    let workspace_doc = documents.get("workspace3")
        .ok_or_else(|| unsafe_data("private execution has no workspace owner binding"))?;
    let (aliases, workspace) = execution_plan(documents.keys().map(String::as_str), workspace_doc, manifest, principal)?;
    if aliases.is_empty() { return Ok(aliases); }
    let mut replacements = BTreeMap::new();
    for canonical in aliases.values().collect::<BTreeSet<_>>() {
        let merged = reconcile_execution_group(canonical, &aliases, &workspace, manifest, principal, |id| {
            documents.get(id).map(execution_root).transpose()
        })?;
        replacements.insert(canonical.clone(), merged);
    }
    replacements.insert("workspace3".into(), materialize_workspace(&workspace)?);
    for alias in aliases.keys() { documents.remove(alias); }
    documents.extend(replacements);
    Ok(aliases)
}

pub(crate) struct PreparedExecutionSnapshots {
    pub connection: rusqlite::Connection,
    _file: tempfile::NamedTempFile,
    pub execution_aliases: BTreeMap<String, String>,
}

/// Temporary private staging only; original snapshot bytes remain untouched in `source`.
pub(crate) fn prepare_execution_snapshots(
    source: &rusqlite::Connection,
    temporary_parent: &std::path::Path,
    manifest: &CrewBackupManifest,
    principal: &CollaborationPrincipal,
    excluded: &BTreeSet<String>,
) -> Result<PreparedExecutionSnapshots> {
    let ids = inert_snapshot_ids(source)?;
    let file = tempfile::NamedTempFile::new_in(temporary_parent)?;
    let mut connection = rusqlite::Connection::open(file.path())?;
    connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA cache_size=-4096; PRAGMA temp_store=FILE;
        CREATE TABLE snapshots(doc_id TEXT PRIMARY KEY,bytes BLOB NOT NULL) STRICT;")?;
    {
        let transaction = connection.transaction()?;
        let mut statement = source.prepare("SELECT doc_id,bytes FROM snapshots WHERE substr(doc_id,1,length(?1))!=?1")?;
        let mut rows = statement.query(["recovery-original/"])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let bytes: Vec<u8> = row.get(1)?;
            // Validate even excluded records before omitting them from the live staging view.
            decode_execution_doc(&bytes)?;
            if !excluded.contains(&id) {
                transaction.execute("INSERT INTO snapshots VALUES (?1,?2)", rusqlite::params![id, bytes])?;
            }
        }
        transaction.commit()?;
    }
    let workspace_doc = load_execution_doc(&connection, "workspace3")?;
    let (execution_aliases, workspace) = execution_plan(
        ids.iter().filter(|id| !id.starts_with("recovery-original/") && !excluded.contains(*id)).map(String::as_str),
        &workspace_doc, manifest, principal)?;
    drop(workspace_doc);
    if !execution_aliases.is_empty() {
        for canonical in execution_aliases.values().collect::<BTreeSet<_>>() {
            let merged = reconcile_execution_group(canonical, &execution_aliases, &workspace, manifest, principal, |id| {
                use rusqlite::OptionalExtension;
                let bytes: Option<Vec<u8>> = connection.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [id], |row| row.get(0)).optional()?;
                bytes.map(|bytes| execution_root(&decode_execution_doc(&bytes)?)).transpose()
            })?;
            let bytes = merged.export(loro::ExportMode::Snapshot).map_err(|error| zeron_doc::DocError::Schema(error.to_string()))?;
            connection.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id=?2", rusqlite::params![bytes, canonical])?;
            for (alias, target) in &execution_aliases {
                if target == canonical { connection.execute("DELETE FROM snapshots WHERE doc_id=?1", [alias])?; }
            }
        }
        let bytes = materialize_workspace(&workspace)?.export(loro::ExportMode::Snapshot)
            .map_err(|error| zeron_doc::DocError::Schema(error.to_string()))?;
        connection.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id='workspace3'", [bytes])?;
    }
    Ok(PreparedExecutionSnapshots { _file: file, connection, execution_aliases })
}

pub(crate) fn load_execution_doc(db: &rusqlite::Connection, id: &str) -> Result<LoroDoc> {
    let bytes: Vec<u8> = db.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [id], |row| row.get(0))?;
    decode_execution_doc(&bytes)
}

fn decode_execution_doc(bytes: &[u8]) -> Result<LoroDoc> {
    let doc = LoroDoc::new();
    let status = doc.import(bytes).map_err(|error| zeron_doc::DocError::Schema(error.to_string()))?;
    if status.pending.is_some() { return Err(unsafe_data("snapshot has unresolved causal dependencies")); }
    complete(&doc)?;
    Ok(doc)
}

fn execution_root(doc: &LoroDoc) -> Result<Value> {
    complete(doc)?;
    Ok(doc.get_deep_value().to_json_value())
}

fn execution_plan<'a>(
    ids: impl IntoIterator<Item = &'a str>,
    workspace_doc: &LoroDoc,
    manifest: &CrewBackupManifest,
    principal: &CollaborationPrincipal,
) -> Result<(BTreeMap<String, String>, Value)> {
    let mut aliases = BTreeMap::new();
    for key in ids.into_iter().filter(|key| key.contains("::session::")) {
        let canonical = canonical_execution_id(key)
            .ok_or_else(|| unsafe_data(format!("distinct or invalid private execution identity: {key}")))?;
        aliases.insert(key.to_owned(), canonical.to_owned());
    }
    if principal.session_id.is_some() || principal.subject != manifest.owner_subject
        || principal.project_id != manifest.project_id || principal.deployment_id != manifest.deployment_id {
        return Err(unsafe_data("private execution import requires verified manifest owner scope"));
    }
    let mut workspace = workspace_doc.get_deep_value().to_json_value();
    normalize_workspace(workspace_doc, &mut workspace, &mut aliases, manifest, principal)?;
    if !aliases.is_empty() { complete(workspace_doc)?; }
    Ok((aliases, workspace))
}

fn reconcile_execution_group(
    canonical: &str,
    aliases: &BTreeMap<String, String>,
    workspace: &Value,
    manifest: &CrewBackupManifest,
    principal: &CollaborationPrincipal,
    mut load: impl FnMut(&str) -> Result<Option<Value>>,
) -> Result<LoroDoc> {
    let binding = manifest.chats.get(canonical)
        .ok_or_else(|| unsafe_data(format!("private execution has no canonical manifest binding: {canonical}")))?;
    binding.validate().map_err(unsafe_data)?;
    if binding.session_id != canonical || binding.owner_subject != principal.subject
        || binding.scope.project_id != principal.project_id || binding.scope.deployment_id != principal.deployment_id {
        return Err(unsafe_data(format!("private execution canonical owner/scope mismatch: {canonical}")));
    }
    let owner_device = workspace.get("chats").and_then(|rows| rows.get(canonical))
        .and_then(|row| row.get("deviceId")).and_then(Value::as_str).filter(|device| !device.is_empty())
        .ok_or_else(|| unsafe_data(format!("private execution has no canonical workspace device: {canonical}")))?;
    let mut root = load(canonical)?
        .ok_or_else(|| unsafe_data(format!("private execution has no canonical snapshot: {canonical}")))?;
    validate_source(&mut root, canonical, canonical, principal, owner_device)?;
    for (alias, target) in aliases {
        if target != canonical { continue; }
        let Some(mut source) = load(alias)? else { continue; };
        validate_source(&mut source, alias, canonical, principal, owner_device)?;
        merge_root(&mut root, source)?;
    }
    materialize(&root)
}

fn complete(doc: &LoroDoc) -> Result<()> {
    if doc.is_detached() || !state_matches_oplog(doc, &doc.oplog_vv())? {
        return Err(unsafe_data("incomplete private execution snapshot"));
    }
    Ok(())
}

fn normalize_id(value: &mut Value, canonical: &str) -> Result<()> {
    let id = value
        .as_str()
        .ok_or_else(|| unsafe_data("execution identity is not a string"))?;
    if id != canonical && canonical_execution_id(id) != Some(canonical) {
        return Err(unsafe_data(format!(
            "shared or foreign execution membership: {id}"
        )));
    }
    *value = canonical.into();
    Ok(())
}

fn validate_source(
    root: &mut Value,
    alias: &str,
    canonical: &str,
    principal: &CollaborationPrincipal,
    device: &str,
) -> Result<()> {
    let meta = root
        .get_mut("meta")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| unsafe_data("execution snapshot has no metadata"))?;
    if !matches!(
        meta.get("schemaVersion").and_then(Value::as_u64),
        Some(1 | 2)
    ) {
        return Err(unsafe_data("unsupported private execution schema"));
    }
    let id = meta
        .get_mut("chatId")
        .ok_or_else(|| unsafe_data("execution snapshot has no chatId"))?;
    // A filename is not authority: validate the original identity before rewriting.
    if id.as_str() != Some(alias) && id.as_str() != Some(canonical) {
        return Err(unsafe_data(
            "execution snapshot has a different logical owner",
        ));
    }
    *id = canonical.into();
    if let Some(messages) = root.get_mut("messages") {
        for message in messages
            .as_array_mut()
            .ok_or_else(|| unsafe_data("invalid execution messages"))?
        {
            if message.get("role").and_then(Value::as_str) == Some("assistant")
                && message.get("deviceId").and_then(Value::as_str) != Some(device)
            {
                return Err(unsafe_data("private output belongs to another device"));
            }
            if let Some(peer) = message
                .get_mut("peerMessage")
                .and_then(|peer| peer.get_mut("sourceChatId"))
            {
                if peer
                    .as_str()
                    .is_some_and(|id| canonical_execution_id(id) == Some(canonical))
                {
                    *peer = canonical.into();
                } else if peer.as_str().is_some_and(|id| id.contains("::session::")) {
                    return Err(unsafe_data("unresolved private peer provenance"));
                }
            }
        }
    }
    if let Some(publications) = root.get_mut("publications") {
        for row in publications
            .as_array_mut()
            .ok_or_else(|| unsafe_data("invalid execution publications"))?
        {
            let old_id = row
                .get("id")
                .cloned()
                .ok_or_else(|| unsafe_data("publication without identity"))?;
            let record = row
                .get_mut("record")
                .ok_or_else(|| unsafe_data("publication without record"))?;
            if record.get("id") != Some(&old_id) {
                return Err(unsafe_data("publication identity mismatch"));
            }
            normalize_publication(record, alias, canonical, principal, device)?;
            let normalized_id = record["id"].clone();
            row["id"] = normalized_id;
        }
    }
    if let Some(register) = root.get_mut("agentSessions") {
        let old = std::mem::take(
            register
                .as_object_mut()
                .ok_or_else(|| unsafe_data("invalid agent session register"))?,
        );
        for (key, mut record) in old {
            let mut normalized = Value::String(key);
            normalize_id(&mut normalized, canonical)?;
            if record.get("kind").and_then(Value::as_str) != Some("agentSession") {
                return Err(unsafe_data("non-session private owner register"));
            }
            normalize_publication(&mut record, alias, canonical, principal, device)?;
            let rows = register.as_object_mut().expect("validated register");
            if let Some(previous) = rows.insert(canonical.into(), record.clone()) {
                if previous != record {
                    return Err(unsafe_data("conflicting private owner registers"));
                }
            }
        }
    }
    Ok(())
}

fn normalize_publication(
    record: &mut Value,
    alias: &str,
    canonical: &str,
    principal: &CollaborationPrincipal,
    device: &str,
) -> Result<()> {
    let owner = principal.subject.as_str();
    let publication: PublicationRecord = serde_json::from_value(record.clone())?;
    if publication.published_by != owner {
        return Err(unsafe_data("foreign private publication owner"));
    }
    match publication.value {
        PublicationValue::MessageProvenance(provenance) => {
            if provenance.author_subject != owner || provenance.owner_device_id != device {
                return Err(unsafe_data("foreign private message provenance"));
            }
            normalize_id(&mut record["value"]["sessionId"], canonical)?;
            let normalized = if canonical_execution_id(&provenance.session_id).is_some() {
                publication.id.replace(&provenance.session_id, canonical)
            } else {
                publication.id
            };
            record["id"] = normalized.replace(alias, canonical).into();
        }
        PublicationValue::AgentSession(session) => {
            if session.owner_subject != owner || session.owner_device_id != device {
                return Err(unsafe_data("foreign private agent membership"));
            }
            normalize_id(&mut record["value"]["sessionId"], canonical)?;
            normalize_id(&mut record["value"]["chatId"], canonical)?;
            // Environment grants are not alias-rewritable authority.
            if session.environment.as_ref().is_some_and(|environment| {
                environment.owner_principal != owner
                    || environment.scope.session_id.as_deref() != Some(canonical)
                    || environment.scope.project_id != principal.project_id
                    || environment.scope.deployment_id != principal.deployment_id
            }) {
                return Err(unsafe_data(
                    "private environment requires authenticated reattachment",
                ));
            }
        }
        PublicationValue::Unknown { .. } => {
            return Err(unsafe_data("unknown private publication kind"));
        }
        _ => {}
    }
    let normalized: PublicationRecord = serde_json::from_value(record.clone())?;
    zeron_doc::validate_publication(&normalized)?;
    Ok(())
}

fn merge_root(target: &mut Value, source: Value) -> Result<()> {
    let target = target
        .as_object_mut()
        .ok_or_else(|| unsafe_data("invalid canonical document root"))?;
    let applied: BTreeSet<String> = target
        .get("commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|row| row.get("status").and_then(Value::as_str) == Some("applied"))
        .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    for (container, value) in source
        .as_object()
        .ok_or_else(|| unsafe_data("invalid execution document root"))?
    {
        match container.as_str() {
            "messages" | "commands" | "publications" => {
                let rows = target
                    .entry(container.clone())
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .ok_or_else(|| unsafe_data("invalid canonical list"))?;
                let mut ids = BTreeMap::new();
                for (index, row) in rows.iter().enumerate() {
                    if ids.insert(record_id(row)?.to_owned(), index).is_some() {
                        return Err(unsafe_data("duplicate canonical record identity"));
                    }
                }
                let mut source_ids = BTreeSet::new();
                for row in value
                    .as_array()
                    .ok_or_else(|| unsafe_data("invalid execution list"))?
                {
                    let id = record_id(row)?;
                    if !source_ids.insert(id) {
                        return Err(unsafe_data("duplicate execution record identity"));
                    }
                    if let Some(index) = ids.get(id).copied() {
                        let current = &mut rows[index];
                        if current == row {
                            continue;
                        }
                        match container.as_str() {
                            "messages" => merge_message(current, row, applied.contains(id))?,
                            "commands" => merge_command(current, row)?,
                            _ => {
                                return Err(unsafe_data(format!(
                                    "conflicting immutable publication: {id}"
                                )));
                            }
                        }
                    } else {
                        ids.insert(id.to_owned(), rows.len());
                        rows.push(row.clone());
                    }
                }
            }
            "meta" | "agentSessions" | "crewAnnotations" => {
                let fields = target
                    .entry(container.clone())
                    .or_insert_with(|| Value::Object(Map::new()))
                    .as_object_mut()
                    .ok_or_else(|| unsafe_data("invalid canonical map"))?;
                for (key, value) in value
                    .as_object()
                    .ok_or_else(|| unsafe_data("invalid execution map"))?
                {
                    match fields.get(key) {
                        Some(current) if current == value => {}
                        Some(current) if container == "meta" && key == "schemaVersion" => {
                            fields.insert(
                                key.clone(),
                                Value::from(
                                    current
                                        .as_u64()
                                        .unwrap_or(0)
                                        .max(value.as_u64().unwrap_or(0)),
                                ),
                            );
                        }
                        Some(_) => {
                            return Err(unsafe_data(format!(
                                "conflicting execution field: {container}/{key}"
                            )));
                        }
                        None => {
                            fields.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
            _ if target.get(container) == Some(value) => {}
            _ => {
                return Err(unsafe_data(format!(
                    "unknown execution container: {container}"
                )));
            }
        }
    }
    Ok(())
}

fn record_id(row: &Value) -> Result<&str> {
    row.get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| unsafe_data("execution record without identity"))
}

fn equal_except(a: &Value, b: &Value, excluded: &[&str]) -> bool {
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else {
        return false;
    };
    a.iter()
        .filter(|(key, _)| !excluded.contains(&key.as_str()))
        .all(|(key, value)| b.get(key) == Some(value))
        && b.keys()
            .filter(|key| !excluded.contains(&key.as_str()))
            .all(|key| a.contains_key(key))
}

fn merge_command(current: &mut Value, incoming: &Value) -> Result<()> {
    let terminal = |value: &Value| {
        matches!(
            value.get("status").and_then(Value::as_str),
            Some("applied" | "rejected" | "expired" | "superseded" | "cancelled")
        )
    };
    if equal_except(current, incoming, &["status", "resolution"]) {
        if current["status"] == "pending"
            && current.get("resolution").is_none_or(Value::is_null)
            && terminal(incoming)
        {
            *current = incoming.clone();
            return Ok(());
        }
        if incoming["status"] == "pending"
            && incoming.get("resolution").is_none_or(Value::is_null)
            && terminal(current)
        {
            return Ok(());
        }
    }
    Err(unsafe_data("conflicting private durable command outcome"))
}

fn merge_message(current: &mut Value, incoming: &Value, applied: bool) -> Result<()> {
    let role = current.get("role").and_then(Value::as_str);
    if role == Some("user")
        && incoming["role"] == "user"
        && current["parts"] == incoming["parts"]
        && applied
        && equal_except(current, incoming, &["createdAt", "status"])
    {
        return Ok(()); // Legacy accepted user command owns its authoritative clock.
    }
    if !equal_except(current, incoming, &["parts", "status"]) {
        return Err(unsafe_data("conflicting private message identity"));
    }
    let before = current.get("status").and_then(Value::as_str);
    let after = incoming.get("status").and_then(Value::as_str);
    if role != Some("assistant") && current["parts"] == incoming["parts"] {
        match (before, after) {
            (Some("complete" | "aborted"), Some("queued" | "steered") | None)
            | (Some("steered"), Some("queued") | None) => return Ok(()),
            (Some("queued" | "steered") | None, Some("complete" | "aborted" | "steered")) => {
                current["status"] = incoming["status"].clone();
                return Ok(());
            }
            _ => {}
        }
    }
    if role == Some("assistant") {
        let old = current["parts"]
            .as_array()
            .ok_or_else(|| unsafe_data("invalid message parts"))?;
        let new = incoming["parts"]
            .as_array()
            .ok_or_else(|| unsafe_data("invalid message parts"))?;
        if matches!(before, Some("complete" | "aborted"))
            && after == Some("streaming")
            && parts_prefix(new, old)
        {
            return Ok(());
        }
        if before == Some("streaming") {
            if after == Some("streaming") && parts_prefix(new, old) {
                return Ok(());
            }
            if matches!(after, Some("streaming" | "complete" | "aborted")) && parts_prefix(old, new)
            {
                *current = incoming.clone();
                return Ok(());
            }
            if after == Some("aborted")
                && new.last().is_some_and(|part| part["kind"] == "error")
                && parts_prefix(&new[..new.len() - 1], old)
            {
                let error = new.last().expect("checked error");
                if let Some(existing) = old.iter().find(|part| part.get("id") == error.get("id")) {
                    if existing != error {
                        return Err(unsafe_data("conflicting recovery interruption"));
                    }
                } else {
                    current["parts"]
                        .as_array_mut()
                        .expect("validated parts")
                        .push(error.clone());
                }
                current["status"] = "aborted".into();
                return Ok(());
            }
        }
    }
    // Unlike live legacy recovery, never drop richer streaming content in favor
    // of an older terminal snapshot. Offline ambiguity requires reconciliation.
    Err(unsafe_data("conflicting private transcript content"))
}

fn parts_prefix(old: &[Value], new: &[Value]) -> bool {
    old.len() <= new.len()
        && old.iter().zip(new).all(|(old, new)| {
            if old == new {
                return true;
            }
            if old.get("id") != new.get("id") || old.get("kind") != new.get("kind") {
                return false;
            }
            match old.get("kind").and_then(Value::as_str) {
                Some("text") => {
                    equal_except(old, new, &["text"])
                        && old["text"]
                            .as_str()
                            .zip(new["text"].as_str())
                            .is_some_and(|(old, new)| new.starts_with(old))
                }
                Some("input") => {
                    equal_except(old, new, &["resolved"])
                        && (old["resolved"] == false || new["resolved"] == true)
                }
                Some("thinking") => {
                    equal_except(old, new, &["started", "completed"])
                        && ["started", "completed"]
                            .iter()
                            .all(|key| old[*key] == false || new[*key] == true)
                }
                Some("tool") => {
                    equal_except(old, new, &["call", "isError", "resolved"])
                        && (old["resolved"] == false
                            || (new["resolved"] == true
                                && old["isError"] == new["isError"]
                                && tool_call_prefix(&old["call"], &new["call"])))
                }
                _ => false,
            }
        })
}

fn tool_call_prefix(old: &Value, new: &Value) -> bool {
    if old == new {
        return true;
    }
    if old["kind"] != "todo" || new["kind"] != "todo" || !equal_except(old, new, &["items"]) {
        return false;
    }
    let (Some(old), Some(new)) = (old["items"].as_array(), new["items"].as_array()) else {
        return false;
    };
    old.len() <= new.len()
        && old.iter().zip(new).all(|(old, new)| {
            equal_except(old, new, &["done"]) && (old["done"] == false || new["done"] == true)
        })
}

fn materialize(root: &Value) -> Result<LoroDoc> {
    let doc = LoroDoc::new();
    for (key, value) in root
        .as_object()
        .ok_or_else(|| unsafe_data("invalid reconciled root"))?
    {
        match value {
            Value::Object(fields) => write_map(&doc.get_map(key.as_str()), fields)?,
            Value::Array(rows) => write_list(&doc.get_list(key.as_str()), rows)?,
            _ => return Err(unsafe_data("unsupported reconciled root type")),
        }
    }
    doc.commit();
    Ok(doc)
}

fn write_map(map: &LoroMap, fields: &Map<String, Value>) -> Result<()> {
    for (key, value) in fields {
        match value {
            Value::Array(rows) if key == "parts" => {
                write_list(&map.insert_container(key, LoroList::new())?, rows)?
            }
            Value::String(text) if matches!(key.as_str(), "text" | "reasoning") => {
                map.insert_container(key, LoroText::new())?
                    .insert(0, text)?;
            }
            _ => {
                map.insert(key, LoroValue::from(value.clone()))?;
            }
        }
    }
    Ok(())
}

fn write_list(list: &LoroList, rows: &[Value]) -> Result<()> {
    for row in rows {
        let fields = row
            .as_object()
            .ok_or_else(|| unsafe_data("non-object semantic list row"))?;
        write_map(&list.push_container(LoroMap::new())?, fields)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn causal_state_walk_preserves_branch_dependencies_partial_frontiers_and_source() {
        use loro::{ExportMode, Frontiers, ID, VersionVector};
        let base = LoroDoc::new();
        base.set_peer_id(1).unwrap();
        base.get_text("text").insert(0, "abcdef").unwrap();
        base.commit();
        let baseline = base.oplog_vv();
        let left = base.fork();
        left.set_peer_id(2).unwrap();
        left.get_map("meta").insert("left", true).unwrap();
        left.commit();
        let right = base.fork();
        right.set_peer_id(3).unwrap();
        right.get_map("meta").insert("right", true).unwrap();
        right.commit();
        let joined = left.fork();
        joined.set_peer_id(4).unwrap();
        joined.import(&right.export(ExportMode::updates(&baseline)).unwrap()).unwrap();
        joined.get_map("meta").insert("joined", true).unwrap();
        joined.commit();
        // A newer change on peer 4 must not hide the older change's other-peer deps.
        joined.set_next_commit_message("separate newer change");
        joined.get_map("meta").insert("later", true).unwrap();
        joined.commit();
        let bytes = joined.export(ExportMode::Snapshot).unwrap();
        let doc = LoroDoc::new();
        assert!(doc.import(&bytes).unwrap().pending.is_none());
        let version = doc.oplog_vv();
        let mut expected = VersionVector::default();
        for (peer, end) in [(1, 6), (2, 1), (3, 1), (4, 2)] { expected.insert(peer, end); }
        assert_eq!(version, expected);
        let before = doc.export(ExportMode::Snapshot).unwrap();
        let frontiers = doc.state_frontiers();
        assert!(state_matches_oplog(&doc, &version).unwrap());
        assert_eq!(doc.state_vv(), version); // Tiny graph only: native oracle.
        assert_eq!(doc.state_frontiers(), frontiers);
        assert_eq!(doc.export(ExportMode::Snapshot).unwrap(), before);

        // Stored aggregate metadata must not be trusted just because frontiers agree.
        let mut inflated = version.clone();
        inflated.insert(99, 1);
        assert!(!state_matches_oplog(&doc, &inflated).unwrap());
        let mut truncated = version.clone();
        truncated.insert(1, 2);
        assert!(state_matches_oplog(&doc, &truncated).is_err());

        // A state frontier inside a multi-atom change is not its whole change.
        doc.checkout(&Frontiers::from_id(ID::new(1, 2))).unwrap();
        assert!(!state_matches_oplog(&doc, &version).unwrap());
        assert!(!state_matches_oplog(&doc, &baseline).unwrap());
        assert_ne!(doc.state_vv(), version);
        assert!(complete(&doc).is_err()); // Detached input remains rejected.
        doc.checkout_to_latest();
        assert!(state_matches_oplog(&doc, &version).unwrap());

        // Accepted update syntax does not imply its absent dependencies exist.
        let missing = right.export(ExportMode::updates(&baseline)).unwrap();
        let incomplete = LoroDoc::new();
        assert!(incomplete.import(&missing).unwrap().pending.is_some());
        assert!(decode_execution_doc(&missing).is_err());

        let shallow = LoroDoc::new();
        shallow.import(&joined.export(ExportMode::shallow_snapshot(&joined.oplog_frontiers())).unwrap()).unwrap();
        assert!(shallow.is_shallow());
        assert_eq!(state_matches_oplog(&shallow, &shallow.oplog_vv()).unwrap(),
            shallow.state_vv() == shallow.oplog_vv());

        // The validator must not implicitly commit caller-owned edits.
        doc.get_map("meta").insert("uncommitted", true).unwrap();
        let pending = doc.get_pending_txn_len();
        assert!(pending > 0);
        let _ = state_matches_oplog(&doc, &doc.oplog_vv());
        assert_eq!(doc.get_pending_txn_len(), pending);
    }

    #[test]
    fn shallow_causal_walk_preserves_trimmed_baseline_and_partial_retained_frontiers() {
        use loro::{ExportMode, Frontiers, ID, VersionVector};
        let source = LoroDoc::new();
        source.set_peer_id(1).unwrap();
        source.get_text("text").insert(0, "abcdef").unwrap();
        source.commit();
        source.set_peer_id(2).unwrap();
        source.get_text("text").insert(6, "xyz").unwrap();
        source.commit();
        let doc = LoroDoc::new();
        doc.import(&source.export(ExportMode::shallow_snapshot(&Frontiers::from_id(ID::new(1, 2)))).unwrap()).unwrap();
        assert!(doc.is_shallow());
        let version = doc.oplog_vv();
        let baseline = doc.shallow_since_vv().to_vv();
        let root = doc.shallow_since_frontiers().as_single().unwrap();
        assert_eq!(root, ID::new(1, 2));
        let root_deps = change_meta(&doc, root).unwrap().unwrap().deps;
        let before = doc.export(ExportMode::Snapshot).unwrap();
        assert!(state_matches_oplog(&doc, &version).unwrap());
        assert_eq!(doc.state_vv(), version); // Tiny native oracle only.
        assert_eq!(doc.export(ExportMode::Snapshot).unwrap(), before);
        assert_eq!(doc.frontiers_to_vv(&root_deps).unwrap(), baseline);
        assert!(frontiers_match_oplog(&doc, &root_deps, &baseline).unwrap());
        assert!(!frontiers_match_oplog(&doc, &root_deps, &version).unwrap());
        assert!(!frontiers_match_oplog(&doc, &Frontiers::default(), &version).unwrap());
        assert!(frontiers_match_oplog(&doc, &Frontiers::from_id(ID::new(1, 0)), &version).is_err());
        for id in [root, ID::new(1, 4), ID::new(2, 1)] {
            doc.checkout(&Frontiers::from_id(id)).unwrap();
            assert!(!state_matches_oplog(&doc, &version).unwrap());
            let expected = doc.state_vv();
            // A supplied prefix can end within a retained change, but the
            // existing full-change span guard still rejects understated history.
            assert_ne!(expected, version);
            assert!(baseline.iter().all(|(peer, end)| expected.get(peer).copied().unwrap_or(0) >= *end));
            assert!(complete(&doc).is_err());
        }
        doc.checkout_to_latest();
        assert!(state_matches_oplog(&doc, &version).unwrap());
        let mut inflated = version.clone();
        inflated.insert(99, 1);
        assert!(!state_matches_oplog(&doc, &inflated).unwrap());
        let missing = ID::new(2, *version.get(&2).unwrap());
        let mut missing_version = version.clone();
        missing_version.extend_to_include_last_id(missing);
        assert!(frontiers_match_oplog(&doc, &Frontiers::from_id(missing), &missing_version).is_err());
        assert!(frontiers_match_oplog(&doc, &root_deps, &VersionVector::default()).is_err());
    }

    #[test]
    fn streamed_inert_classification_matches_loaded_references_and_keeps_source_bytes() {
        use loro::ExportMode;
        let id = "a-generic-bootstrap";
        for scenario in ["orphan", "later-deleted-reference", "typed-physical", "sql", "recovery", "opaque-recovery"] {
            let db = rusqlite::Connection::open_in_memory().unwrap();
            db.execute_batch("CREATE TABLE snapshots(doc_id TEXT PRIMARY KEY,bytes BLOB NOT NULL)").unwrap();
            let candidate = LoroDoc::new();
            candidate.get_map("meta").insert("chatId", id).unwrap();
            candidate.get_map("meta").insert("schemaVersion", 2).unwrap();
            candidate.commit();
            let bytes = candidate.export(ExportMode::Snapshot).unwrap();
            db.execute("INSERT INTO snapshots VALUES (?1,?2)", rusqlite::params![id, &bytes]).unwrap();
            let mut documents = BTreeMap::from([(id.into(), candidate)]);
            let later = LoroDoc::new();
            if scenario == "later-deleted-reference" {
                later.get_map("sessionRefs").insert("prior", LoroValue::from(json!({"chatId":id}))).unwrap();
                later.commit();
                later.get_map("sessionRefs").delete("prior").unwrap();
            } else if scenario == "typed-physical" {
                later.get_map("sessionRefs").insert("actual", LoroValue::from(json!({"chatId":"actual",
                    "environment":{"ownerPrincipal":"owner","scope":{"projectId":"project","sessionId":"actual"},
                        "source":{"kind":"scaffold","sandbox_id":id,"lifecycle":"stopped"}}}))).unwrap();
            }
            later.commit();
            db.execute("INSERT INTO snapshots VALUES ('z-later',?1)", [later.export(ExportMode::Snapshot).unwrap()]).unwrap();
            documents.insert("z-later".into(), later);
            if scenario == "sql" {
                db.execute_batch("CREATE TABLE semantic_intents(doc_id TEXT,value BLOB)").unwrap();
                db.execute("INSERT INTO semantic_intents VALUES ('other',?1)", [serde_json::to_vec(&json!({"chatId":id})).unwrap()]).unwrap();
            }
            if matches!(scenario, "recovery" | "opaque-recovery") {
                let recovered = if scenario == "recovery" { bytes.clone() } else { b"opaque".to_vec() };
                db.execute("INSERT INTO snapshots VALUES ('recovery-original/old',?1)", [recovered]).unwrap();
            }
            let before: Vec<(String, Vec<u8>)> = db.prepare("SELECT doc_id,bytes FROM snapshots ORDER BY doc_id").unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().collect::<std::result::Result<_, _>>().unwrap();
            let loaded = inert_bootstrap_snapshots(&db, &documents, &BTreeSet::new()).unwrap();
            let streamed = inert_bootstrap_snapshots_from_store(&db, &BTreeSet::new()).unwrap();
            assert_eq!(streamed, loaded, "{scenario}");
            assert_eq!(streamed.contains_key(id), matches!(scenario, "orphan" | "typed-physical"), "{scenario}");
            let after: Vec<(String, Vec<u8>)> = db.prepare("SELECT doc_id,bytes FROM snapshots ORDER BY doc_id").unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().collect::<std::result::Result<_, _>>().unwrap();
            assert_eq!(before, after);
        }
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE snapshots(doc_id TEXT,bytes BLOB)").unwrap();
        let source = LoroDoc::new();
        source.get_map("old").insert("value", "trimmed").unwrap();
        source.commit();
        let shallow = source.export(ExportMode::shallow_snapshot(&source.oplog_frontiers())).unwrap();
        db.execute("INSERT INTO snapshots VALUES ('a-shallow',?1)", [shallow]).unwrap();
        db.execute("INSERT INTO snapshots VALUES ('z-malformed',?1)", [b"invalid".as_slice()]).unwrap();
        assert!(inert_bootstrap_snapshots_from_store(&db, &BTreeSet::new()).is_err());
        db.execute("INSERT INTO snapshots VALUES ('a-shallow',?1)", [b"duplicate".as_slice()]).unwrap();
        assert!(inert_bootstrap_snapshots_from_store(&db, &BTreeSet::new()).is_err());
    }

    #[test]
    fn inert_bootstrap_rejects_deleted_cross_document_references_and_unknown_history() {
        use loro::ExportMode;
        let id = "generic-historically-referenced-bootstrap";
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE snapshots(doc_id TEXT PRIMARY KEY, bytes BLOB NOT NULL);")
            .unwrap();
        let candidate = LoroDoc::new();
        candidate.get_map("meta").insert("chatId", id).unwrap();
        candidate
            .get_map("meta")
            .insert("schemaVersion", 2)
            .unwrap();
        candidate.commit();
        let bytes = candidate.export(ExportMode::Snapshot).unwrap();
        db.execute(
            "INSERT INTO snapshots VALUES (?1,?2)",
            rusqlite::params![id, &bytes],
        )
        .unwrap();
        let mut documents = BTreeMap::from([(id.into(), candidate)]);
        let mut incorrectly_classified = Vec::new();
        for scenario in [
            "deleted-reference",
            "deleted-nested-reference",
            "deleted-reference-key",
            "shallow-unknown-history",
        ] {
            let workspace = LoroDoc::new();
            let key = if scenario == "deleted-reference-key" {
                id
            } else {
                "prior-membership"
            };
            if scenario == "deleted-nested-reference" {
                let child = workspace
                    .get_map("sessionRefs")
                    .insert_container(key, LoroMap::new())
                    .unwrap();
                child.insert("chatId", id).unwrap();
                workspace.commit();
                child.delete("chatId").unwrap();
            } else {
                let value = if scenario == "deleted-reference-key" {
                    json!({"chatId":"another-chat"})
                } else {
                    json!({"chatId":id})
                };
                workspace
                    .get_map("sessionRefs")
                    .insert(key, LoroValue::from(value))
                    .unwrap();
                workspace.commit();
                workspace.get_map("sessionRefs").delete(key).unwrap();
            }
            workspace.commit();
            let workspace = if scenario == "shallow-unknown-history" {
                let shallow = LoroDoc::new();
                shallow
                    .import(
                        &workspace
                            .export(ExportMode::shallow_snapshot(&workspace.oplog_frontiers()))
                            .unwrap(),
                    )
                    .unwrap();
                assert!(shallow.is_shallow());
                shallow
            } else {
                workspace
            };
            documents.insert("workspace3".into(), workspace);
            if inert_bootstrap_snapshots(&db, &documents, &BTreeSet::new())
                .unwrap()
                .contains_key(id)
            {
                incorrectly_classified.push(scenario);
            }
        }
        assert!(
            incorrectly_classified.is_empty(),
            "historical association or unknown history was classified inert: {incorrectly_classified:?}"
        );
        assert_eq!(
            db.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [id], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .unwrap(),
            bytes
        );
    }

    #[test]
    fn inert_bootstrap_requires_pristine_history_and_no_semantic_association() {
        use loro::ExportMode;
        use sha2::{Digest, Sha256};
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE snapshots(doc_id TEXT PRIMARY KEY, bytes BLOB NOT NULL);")
            .unwrap();
        let id = "generic-orphan";
        let make = || {
            let doc = LoroDoc::new();
            doc.get_map("meta").insert("chatId", id).unwrap();
            doc.get_map("meta").insert("schemaVersion", 2).unwrap();
            doc.get_map("meta")
                .insert("directoryCompletedTurn", "")
                .unwrap();
            doc.commit();
            doc
        };
        let doc = make();
        let bytes = doc.export(ExportMode::Snapshot).unwrap();
        db.execute(
            "INSERT INTO snapshots VALUES (?1,?2)",
            rusqlite::params![id, &bytes],
        )
        .unwrap();
        let mut docs = BTreeMap::from([(id.to_owned(), doc)]);
        let expected = BTreeMap::from([(id.to_owned(), format!("{:x}", Sha256::digest(&bytes)))]);
        assert_eq!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new()).unwrap(),
            expected
        );
        assert!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::from([id.into()]))
                .unwrap()
                .is_empty()
        );
        for scenario in [
            "content",
            "deleted-content",
            "metadata-history",
            "shallow",
            "extra-root",
        ] {
            let doc = make();
            match scenario {
                "content" => {
                    doc.get_list("messages").push("kept").unwrap();
                }
                "deleted-content" => {
                    doc.get_list("commands").push("old").unwrap();
                    doc.commit();
                    doc.get_list("commands").delete(0, 1).unwrap();
                }
                "metadata-history" => {
                    doc.get_map("meta")
                        .insert("directoryCompletedTurn", "old")
                        .unwrap();
                    doc.commit();
                    doc.get_map("meta")
                        .insert("directoryCompletedTurn", "")
                        .unwrap();
                }
                "extra-root" => {
                    doc.get_map("crewAnnotations")
                        .insert("old", "annotation")
                        .unwrap();
                    doc.get_map("crewAnnotations").delete("old").unwrap();
                }
                _ => {}
            }
            doc.commit();
            let doc = if scenario == "shallow" {
                let shallow = LoroDoc::new();
                shallow
                    .import(
                        &doc.export(ExportMode::shallow_snapshot(&doc.oplog_frontiers()))
                            .unwrap(),
                    )
                    .unwrap();
                shallow
            } else {
                doc
            };
            docs.insert(id.into(), doc);
            assert!(
                inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new())
                    .unwrap()
                    .is_empty(),
                "{scenario}"
            );
        }
        docs.insert(id.into(), make());
        let workspace = LoroDoc::new();
        workspace
            .get_map("sessionRefs")
            .insert("other", LoroValue::from(json!({"chatId":id})))
            .unwrap();
        workspace.commit();
        docs.insert("workspace3".into(), workspace);
        assert!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
        docs.remove("workspace3");
        let environment = json!({"source":{"kind":"scaffold","sandbox_id":id,"lifecycle":"stopped"},
            "ownerPrincipal":"owner","scope":{"projectId":"project","deploymentId":"deployment","sessionId":"actual-chat"}});
        for scenario in [
            "typed-physical",
            "deleted-typed-physical",
            "logical-scope",
            "untyped-physical",
        ] {
            let mut environment = environment.clone();
            if scenario == "logical-scope" {
                environment["scope"]["sessionId"] = json!(id);
            }
            if scenario == "untyped-physical" {
                environment
                    .as_object_mut()
                    .unwrap()
                    .remove("ownerPrincipal");
            }
            let workspace = LoroDoc::new();
            workspace
                .get_map("sessionRefs")
                .insert(
                    "actual-chat",
                    LoroValue::from(json!({"chatId":"actual-chat","environment":environment})),
                )
                .unwrap();
            workspace.commit();
            if scenario == "deleted-typed-physical" {
                workspace
                    .get_map("sessionRefs")
                    .delete("actual-chat")
                    .unwrap();
                workspace.commit();
            }
            docs.insert("workspace3".into(), workspace);
            let classified = inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new()).unwrap();
            assert_eq!(
                classified.contains_key(id),
                matches!(scenario, "typed-physical" | "deleted-typed-physical"),
                "{scenario}"
            );
        }
        docs.remove("workspace3");
        let recovered = make();
        db.execute(
            "INSERT INTO snapshots VALUES ('recovery-original/prior-scope',?1)",
            [recovered.export(ExportMode::Snapshot).unwrap()],
        )
        .unwrap();
        assert!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
        db.execute(
            "UPDATE snapshots SET bytes=?1 WHERE doc_id='recovery-original/prior-scope'",
            [b"opaque retained bytes".as_slice()],
        )
        .unwrap();
        assert!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
        db.execute(
            "DELETE FROM snapshots WHERE doc_id='recovery-original/prior-scope'",
            [],
        )
        .unwrap();
        for (table, identity, payload) in [
            ("document_scopes", "doc_id", Some("scope")),
            ("semantic_intents", "doc_id", Some("value")),
            ("crew_directory_jobs", "session_id", None),
            ("pending_nudges", "chat_id", None),
            ("session_startups", "chat_id", None),
        ] {
            db.execute_batch(&format!(
                "CREATE TABLE {table}({identity} TEXT{});",
                payload.map_or(String::new(), |field| format!(",{field} TEXT"))
            ))
            .unwrap();
            if payload.is_some() {
                db.execute(
                    &format!("INSERT INTO {table} VALUES ('other',?1)"),
                    [json!({"session":id}).to_string()],
                )
                .unwrap();
            } else {
                db.execute(&format!("INSERT INTO {table} VALUES (?1)"), [id])
                    .unwrap();
            }
            assert!(
                inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new())
                    .unwrap()
                    .is_empty(),
                "{table}"
            );
            db.execute_batch(&format!("DROP TABLE {table}")).unwrap();
        }
        db.execute_batch("CREATE TABLE session_startups(id TEXT PRIMARY KEY);")
            .unwrap();
        assert_eq!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new()).unwrap(),
            expected
        );
        db.execute("INSERT INTO session_startups VALUES (?1)", [id])
            .unwrap();
        assert!(inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new()).is_err());
        db.execute_batch("DROP TABLE session_startups").unwrap();
        let retained: Vec<u8> = db
            .query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(retained, bytes);
        assert_eq!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new()).unwrap(),
            expected
        );
    }

    const CHAT: &str = "00000000-0000-4000-8000-000000000213";

    fn fixture() -> (
        BTreeMap<String, LoroDoc>,
        CrewBackupManifest,
        CollaborationPrincipal,
        String,
    ) {
        let alias = format!("{CHAT}::session::{CHAT}::session::{CHAT}");
        let manifest = serde_json::from_value(json!({
            "version":1,"writerStopped":true,"sourceProfileRoot":"/legacy","ownerSubject":"owner",
            "projectId":"project","deploymentId":null,"files":{},"chats":{
                (CHAT):{"ownerSubject":"owner","sessionId":CHAT,"scope":{"projectId":"project","sessionId":CHAT}}
            }
        })).unwrap();
        let principal =
            serde_json::from_value(json!({"subject":"owner","projectId":"project"})).unwrap();
        let root = json!({"meta":{"chatId":CHAT,"schemaVersion":2},"messages":[],"commands":[],"publications":[]});
        let mut documents = BTreeMap::new();
        let workspace = LoroDoc::new();
        workspace
            .get_map("chats")
            .insert(
                CHAT,
                LoroValue::from(json!({"id":CHAT,"deviceId":"device"})),
            )
            .unwrap();
        workspace.commit();
        documents.insert("workspace3".into(), workspace);
        documents.insert(CHAT.into(), materialize(&root).unwrap());
        let mut private = root;
        private["meta"]["chatId"] = alias.clone().into();
        documents.insert(alias.clone(), materialize(&private).unwrap());
        (documents, manifest, principal, alias)
    }

    #[test]
    fn disk_execution_reconciliation_matches_memory_and_preserves_source() {
        use loro::ExportMode;
        use sha2::{Digest, Sha256};
        for foreign in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let source_path = temp.path().join("source.sqlite3");
            let source = rusqlite::Connection::open(&source_path).unwrap();
            source.execute_batch("CREATE TABLE snapshots(doc_id TEXT PRIMARY KEY,bytes BLOB NOT NULL)").unwrap();
            let (mut documents, manifest, principal, alias) = fixture();
            let mut private = documents[&alias].get_deep_value().to_json_value();
            private["messages"] = json!([{"id":"answer","role":"assistant","deviceId":if foreign {"foreign"} else {"device"},
                "parts":[{"id":"text","kind":"text","text":"retained private output"}]}]);
            private["commands"] = json!([{"id":"accepted","status":"applied","payload":{"kind":"interrupt"}}]);
            private["meta"]["harnessSessionId"] = json!("native-history");
            documents.insert(alias.clone(), materialize(&private).unwrap());
            for (id, doc) in &documents {
                source.execute("INSERT INTO snapshots VALUES (?1,?2)", rusqlite::params![id, doc.export(ExportMode::Snapshot).unwrap()]).unwrap();
            }
            source.execute("INSERT INTO snapshots VALUES ('recovery-original/retained',?1)", [b"raw evidence".as_slice()]).unwrap();
            let excluded = LoroDoc::new();
            excluded.get_map("meta").insert("chatId", "inert").unwrap();
            excluded.get_map("meta").insert("schemaVersion", 2).unwrap();
            excluded.commit();
            source.execute("INSERT INTO snapshots VALUES ('inert',?1)", [excluded.export(ExportMode::Snapshot).unwrap()]).unwrap();
            let inert_id = "00000000-0000-4000-8000-000000000999";
            let inert_alias = format!("{inert_id}::session::{inert_id}");
            let inert_doc = LoroDoc::new();
            inert_doc.get_map("meta").insert("chatId", inert_alias.as_str()).unwrap();
            inert_doc.get_map("meta").insert("schemaVersion", 2).unwrap();
            inert_doc.commit();
            source.execute("INSERT INTO snapshots VALUES (?1,?2)", rusqlite::params![&inert_alias, inert_doc.export(ExportMode::Snapshot).unwrap()]).unwrap();
            let before = Sha256::digest(std::fs::read(&source_path).unwrap());
            let staged = prepare_execution_snapshots(&source, temp.path(), &manifest, &principal, &BTreeSet::from(["inert".into(), inert_alias.clone()]));
            let memory = reconcile_execution_snapshots(&mut documents, &manifest, &principal);
            if foreign {
                assert!(staged.is_err());
                assert!(memory.is_err());
            } else {
                let staged = staged.unwrap();
                assert_eq!(staged.execution_aliases, memory.unwrap());
                assert!(!staged.execution_aliases.contains_key(&inert_alias));
                assert!(staged._file.path().exists());
                let ids: BTreeSet<String> = staged.connection.prepare("SELECT doc_id FROM snapshots").unwrap()
                    .query_map([], |row| row.get(0)).unwrap().collect::<std::result::Result<_, _>>().unwrap();
                assert_eq!(ids, documents.keys().cloned().collect());
                for (id, expected) in &documents {
                    assert_eq!(load_execution_doc(&staged.connection, id).unwrap().get_deep_value(), expected.get_deep_value());
                }
            }
            assert_eq!(Sha256::digest(std::fs::read(&source_path).unwrap()), before);
            source.execute("UPDATE snapshots SET bytes=?1 WHERE doc_id='inert'", [b"invalid excluded snapshot".as_slice()]).unwrap();
            assert!(prepare_execution_snapshots(&source, temp.path(), &manifest, &principal, &BTreeSet::from(["inert".into()])).is_err());
        }
    }

    #[test]
    fn preserves_private_records_and_native_associations() {
        let (mut documents, manifest, principal, alias) = fixture();
        let mut private = documents[&alias].get_deep_value().to_json_value();
        private["messages"] = json!([{"id":"answer","role":"assistant","deviceId":"device","createdAt":1,"status":"complete",
            "parts":[{"id":"text","kind":"text","text":"private output"},{"id":"think","kind":"thinking","started":true,"completed":true}]}]);
        private["commands"] =
            json!([{"id":"accepted","status":"applied","payload":{"kind":"interrupt"}}]);
        private["crewAnnotations"] =
            json!({"note":{"id":"note","body":"keep","anchor":{"messageId":"answer"}}});
        private["meta"]["harnessSessionId"] = "native-history".into();
        documents.insert(alias.clone(), materialize(&private).unwrap());
        assert_eq!(
            reconcile_execution_snapshots(&mut documents, &manifest, &principal).unwrap(),
            BTreeMap::from([(alias.clone(), CHAT.into())])
        );
        assert!(!documents.contains_key(&alias));
        let result = documents[CHAT].get_deep_value().to_json_value();
        assert_eq!(result["messages"], private["messages"]);
        assert_eq!(result["commands"], private["commands"]);
        assert_eq!(result["crewAnnotations"], private["crewAnnotations"]);
        assert_eq!(result["meta"]["harnessSessionId"], "native-history");
        assert_eq!(result["meta"]["chatId"], CHAT);
    }

    #[test]
    fn foreign_ownership_and_distinct_children_fail_without_mutation() {
        for foreign in [false, true] {
            let (mut documents, manifest, principal, alias) = fixture();
            if foreign {
                let mut private = documents[&alias].get_deep_value().to_json_value();
                private["messages"] =
                    json!([{"id":"foreign","role":"assistant","deviceId":"other","parts":[]}]);
                documents.insert(alias.clone(), materialize(&private).unwrap());
            } else {
                let doc = documents.remove(&alias).unwrap();
                documents.insert(
                    format!("{CHAT}::session::00000000-0000-4000-8000-000000000214"),
                    doc,
                );
            }
            let before: BTreeMap<_, _> = documents
                .iter()
                .map(|(id, doc)| (id.clone(), doc.get_deep_value().to_json_value()))
                .collect();
            assert!(reconcile_execution_snapshots(&mut documents, &manifest, &principal).is_err());
            assert_eq!(
                before,
                documents
                    .iter()
                    .map(|(id, doc)| (id.clone(), doc.get_deep_value().to_json_value()))
                    .collect()
            );
        }
    }

    #[test]
    fn reconciles_stream_prefixes_but_rejects_terminal_or_command_conflicts() {
        let mut old = json!({"id":"m","role":"assistant","deviceId":"device","status":"streaming","parts":[{"id":"p","kind":"text","text":"a"}]});
        let new = json!({"id":"m","role":"assistant","deviceId":"device","status":"complete","parts":[{"id":"p","kind":"text","text":"abc"}]});
        merge_message(&mut old, &new, false).unwrap();
        assert_eq!(old, new);
        let mut conflicting = new.clone();
        conflicting["parts"][0]["text"] = "different".into();
        assert!(merge_message(&mut old, &conflicting, false).is_err());
        let mut pending = json!({"id":"command","status":"pending","payload":{"kind":"interrupt"}});
        let applied = json!({"id":"command","status":"applied","payload":{"kind":"interrupt"}});
        merge_command(&mut pending, &applied).unwrap();
        assert_eq!(pending, applied);
        let mut rejected = applied;
        rejected["status"] = "rejected".into();
        assert!(merge_command(&mut pending, &rejected).is_err());
    }

    #[test]
    fn validates_provenance_before_projecting_and_preserves_publications() {
        for foreign in [false, true] {
            let (mut documents, manifest, principal, alias) = fixture();
            let record = json!({"id":format!("provenance/{alias}/message"),"schemaVersion":2,"publishedAt":1,
                "publishedBy":"owner","kind":"messageProvenance","value":{"messageId":"message","sessionId":alias,
                "authorSubject":if foreign {"foreign"} else {"owner"},"ownerDeviceId":"device","source":"local"}});
            let mut private = documents[&alias].get_deep_value().to_json_value();
            private["publications"] = json!([{"id":record["id"],"record":record}]);
            documents.insert(alias.clone(), materialize(&private).unwrap());
            let result = reconcile_execution_snapshots(&mut documents, &manifest, &principal);
            if foreign {
                assert!(result.is_err());
                assert_eq!(
                    documents[&alias].get_deep_value().to_json_value()["meta"]["chatId"],
                    alias
                );
            } else {
                result.unwrap();
                let normalized = normalized_execution_record(
                    "publications",
                    &private["publications"][0],
                    &alias,
                    CHAT,
                )
                .unwrap();
                assert_eq!(
                    documents[CHAT].get_deep_value().to_json_value()["publications"][0],
                    normalized
                );
                assert_eq!(normalized["record"]["value"]["sessionId"], CHAT);
            }
        }
    }

    #[test]
    fn workspace_aliases_preserve_native_binding_and_reject_conflicts() {
        for conflict in [false, true] {
            let (mut documents, manifest, principal, alias) = fixture();
            let mut root = documents["workspace3"].get_deep_value().to_json_value();
            root["chats"][&alias] =
                json!({"id":alias,"deviceId":"device","harnessSessionId":"native"});
            root["sessionRefs"] =
                json!({(format!("5:owner:{alias}")):{"chatId":alias,"userId":"owner"}});
            if conflict {
                root["chats"][CHAT]["harnessSessionId"] = "other".into();
            }
            documents.insert("workspace3".into(), materialize_workspace(&root).unwrap());
            let result = reconcile_execution_snapshots(&mut documents, &manifest, &principal);
            if conflict {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap().get(&alias).map(String::as_str), Some(CHAT));
                let root = documents["workspace3"].get_deep_value().to_json_value();
                assert_eq!(root["chats"][CHAT]["harnessSessionId"], "native");
                assert!(root["chats"].get(&alias).is_none());
                assert_eq!(
                    root["sessionRefs"][format!("5:owner:{CHAT}")]["chatId"],
                    CHAT
                );
            }
        }
    }

    #[test]
    fn journal_projection_is_in_place_and_ignores_other_chats() {
        let alias = format!("{CHAT}::session::{CHAT}");
        let other = "00000000-0000-4000-8000-000000000214";
        for (container, mut row) in [
            ("chats", json!({"id":other,"title":alias})),
            ("sessionRefs", json!({"chatId":other,"userId":"owner"})),
            (
                "publications",
                json!({"id":"provenance","kind":"messageProvenance","value":{"sessionId":other}}),
            ),
        ] {
            let before = row.clone();
            normalize_execution_record(container, &mut row, &alias, CHAT).unwrap();
            assert_eq!(row, before);
        }
        let mut reference = json!({"chatId":alias,"userId":"owner"});
        normalize_execution_record("sessionRefs", &mut reference, &alias, CHAT).unwrap();
        assert_eq!(reference["chatId"], CHAT);
        let mut publication = json!({"id":format!("provenance/{alias}/message"),"kind":"messageProvenance",
            "value":{"sessionId":alias,"authorSubject":"owner"}});
        normalize_execution_record("publications", &mut publication, &alias, CHAT).unwrap();
        assert_eq!(publication["id"], format!("provenance/{CHAT}/message"));
        assert_eq!(publication["value"]["authorSubject"], "owner");
        let projected = publication.clone();
        normalize_execution_record("publications", &mut publication, &alias, CHAT).unwrap();
        assert_eq!(publication, projected);
    }
}

/// Project understood journal identity fields, never owner fields. The caller
/// compares this row to an independently owner-checked snapshot and keeps raw bytes.
pub(crate) fn normalize_execution_record(
    container: &str,
    row: &mut Value,
    alias: &str,
    canonical: &str,
) -> Result<()> {
    if canonical_execution_id(alias) != Some(canonical) {
        return Err(unsafe_data("invalid journal execution alias"));
    }
    let project = |value: &mut Value| {
        if value.as_str().is_some_and(|id| {
            id.starts_with(canonical) && canonical_execution_id(id) == Some(canonical)
        }) {
            *value = canonical.into();
        }
    };
    match container {
        "chats" => {
            if let Some(id) = row.get_mut("id") {
                project(id);
            }
        }
        "sessions" | "sessionRefs" | "worktreeDeletions" => {
            if let Some(id) = row.get_mut("chatId") {
                project(id);
            }
        }
        "messages" => {
            if let Some(peer) = row
                .get_mut("peerMessage")
                .and_then(|peer| peer.get_mut("sourceChatId"))
            {
                project(peer);
            }
        }
        "publications" | "agentSessions" => {
            let wrapped = row.get("record").is_some();
            let record = if wrapped {
                &mut row["record"]
            } else {
                &mut *row
            };
            match record.get("kind").and_then(Value::as_str) {
                Some("messageProvenance") => {
                    let Some(old) = record
                        .get("value")
                        .and_then(|value| value.get("sessionId"))
                        .and_then(Value::as_str)
                    else {
                        return Ok(());
                    };
                    if old != canonical
                        && (!old.starts_with(canonical)
                            || canonical_execution_id(old) != Some(canonical))
                    {
                        return Ok(());
                    }
                    let id = record
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| unsafe_data("journal publication has no ID"))?;
                    if old == canonical && !id.contains(alias) {
                        return Ok(());
                    }
                    let id = if old != canonical {
                        id.replace(old, canonical)
                    } else {
                        id.to_owned()
                    };
                    let id = id.replace(alias, canonical);
                    record["value"]["sessionId"] = canonical.into();
                    record["id"] = id.into();
                }
                Some("agentSession") => {
                    if let Some(value) = record.get_mut("value") {
                        for field in ["sessionId", "chatId"] {
                            if let Some(id) = value.get_mut(field) {
                                project(id);
                            }
                        }
                    }
                    return Ok(()); // Agent membership projection does not rename publication IDs.
                }
                _ => return Ok(()),
            }
            if wrapped {
                row["id"] = row["record"]["id"].clone();
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn normalized_execution_record(
    container: &str,
    value: &Value,
    alias: &str,
    canonical: &str,
) -> Result<Value> {
    let mut row = value.clone();
    normalize_execution_record(container, &mut row, alias, canonical)?;
    Ok(row)
}

fn normalize_workspace(
    original: &LoroDoc,
    workspace: &mut Value,
    aliases: &mut BTreeMap<String, String>,
    manifest: &CrewBackupManifest,
    principal: &CollaborationPrincipal,
) -> Result<()> {
    let owner_devices: BTreeMap<String, String> = workspace
        .get("chats")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|rows| rows.iter())
        .filter_map(|(id, row)| {
            row.get("deviceId")
                .and_then(Value::as_str)
                .map(|device| (id.clone(), device.to_owned()))
        })
        .collect();
    for container in ["chats", "sessions", "sessionRefs", "worktreeDeletions"] {
        let Some(value) = workspace.get_mut(container) else {
            continue;
        };
        let rows = value
            .as_object_mut()
            .ok_or_else(|| unsafe_data("invalid workspace alias container"))?;
        let keys: Vec<_> = rows.keys().cloned().collect();
        for key in keys {
            let row = &rows[&key];
            let identity = if container == "chats" { "id" } else { "chatId" };
            let id = row
                .get(identity)
                .and_then(Value::as_str)
                .ok_or_else(|| unsafe_data("workspace row has no identity"))?;
            let prefix = if container == "sessionRefs" {
                let user = row
                    .get("userId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| unsafe_data("session ref has no user"))?;
                format!("{}:{user}:", user.len())
            } else {
                String::new()
            };
            let key_id = key
                .strip_prefix(&prefix)
                .ok_or_else(|| unsafe_data("invalid session reference key"))?;
            if !key_id.contains("::session::") && !id.contains("::session::") {
                continue;
            }
            let canonical = canonical_execution_id(key_id)
                .or_else(|| canonical_execution_id(id))
                .ok_or_else(|| {
                    unsafe_data("distinct child workspace identity requires reattachment")
                })?
                .to_owned();
            for retained in [key_id, id] {
                if retained != canonical
                    && canonical_execution_id(retained) != Some(canonical.as_str())
                {
                    return Err(unsafe_data("workspace alias identity conflicts"));
                }
                if retained != canonical {
                    aliases.insert(retained.into(), canonical.clone());
                }
            }
            if !manifest.chats.contains_key(&canonical) {
                return Err(unsafe_data(
                    "workspace alias has no canonical manifest binding",
                ));
            }
            for field in ["userId", "ownerSubject", "ownerPrincipal"] {
                if row
                    .get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|owner| owner != principal.subject)
                {
                    return Err(unsafe_data("foreign workspace alias owner"));
                }
            }
            if let Some(environment) = row.get("environment").filter(|value| !value.is_null()) {
                let environment: zeron_proto::SessionEnvironment =
                    serde_json::from_value(environment.clone())?;
                if environment.owner_principal != principal.subject
                    || environment.scope.project_id != principal.project_id
                    || environment.scope.deployment_id != principal.deployment_id
                    || environment.scope.session_id.as_deref() != Some(canonical.as_str())
                {
                    return Err(unsafe_data(
                        "workspace alias environment requires authenticated reattachment",
                    ));
                }
            }
            let owner_field = match container {
                "sessionRefs" => "userId",
                "worktreeDeletions" => "ownerDeviceId",
                _ => "deviceId",
            };
            if row
                .get(owner_field)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(unsafe_data("workspace alias lacks owner identity"));
            }
            if container != "sessionRefs"
                && row.get(owner_field).and_then(Value::as_str)
                    != owner_devices.get(&canonical).map(String::as_str)
            {
                return Err(unsafe_data(
                    "workspace alias device differs from canonical owner",
                ));
            }
            let alias = if canonical_execution_id(key_id).is_some() {
                key_id
            } else {
                id
            };
            let normalized = normalized_execution_record(container, row, alias, &canonical)?;
            let new_key = format!("{prefix}{canonical}");
            if key == new_key {
                rows.insert(new_key, normalized);
                continue;
            }
            if let Some(current) = rows.get_mut(&new_key) {
                let current = current
                    .as_object_mut()
                    .ok_or_else(|| unsafe_data("invalid canonical workspace row"))?;
                let incoming = normalized.as_object().expect("normalized workspace row");
                for field in [
                    "id",
                    "chatId",
                    "userId",
                    "deviceId",
                    "ownerSubject",
                    "ownerDeviceId",
                    "ownerPrincipal",
                    "projectId",
                    "deploymentId",
                ] {
                    if current.get(field) != incoming.get(field) {
                        return Err(unsafe_data("workspace alias owner/scope conflicts"));
                    }
                }
                let newer_status = container == "sessions"
                    && incoming.get("updatedAt").and_then(Value::as_i64)
                        > current.get("updatedAt").and_then(Value::as_i64);
                for (field, value) in incoming {
                    if let Some(existing) = current.get(field) {
                        // Native associations and grants never use display-field precedence.
                        if matches!(
                            field.as_str(),
                            "harnessSessionId"
                                | "harnessSessionCwd"
                                | "forkFrom"
                                | "environment"
                                | "startup"
                                | "crew"
                                | "config"
                        ) && existing != value
                        {
                            return Err(unsafe_data(format!(
                                "workspace alias native/authority conflict: {field}"
                            )));
                        }
                        if newer_status
                            && matches!(field.as_str(), "status" | "startedAt" | "updatedAt")
                        {
                            current.insert(field.clone(), value.clone());
                        }
                    } else {
                        if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                            original.get_map(container).get(&new_key)
                            && map.get_last_editor(field).is_some()
                        {
                            return Err(unsafe_data(
                                "workspace alias conflicts with canonical field tombstone",
                            ));
                        }
                        current.insert(field.clone(), value.clone());
                    }
                }
            } else {
                if container == "chats"
                    || original
                        .get_map(container)
                        .get_last_editor(&new_key)
                        .is_some()
                {
                    return Err(unsafe_data(
                        "workspace alias has no live canonical owner row",
                    ));
                }
                rows.insert(new_key, normalized);
            }
            rows.remove(&key);
        }
    }
    Ok(())
}

fn materialize_workspace(root: &Value) -> Result<LoroDoc> {
    let doc = LoroDoc::new();
    for (container, value) in root
        .as_object()
        .ok_or_else(|| unsafe_data("invalid workspace root"))?
    {
        let fields = value
            .as_object()
            .ok_or_else(|| unsafe_data("invalid workspace map"))?;
        let parent = doc.get_map(container.as_str());
        if container == "meta" {
            write_map(&parent, fields)?;
            continue;
        }
        for (key, row) in fields {
            if let Some(fields) = row.as_object() {
                write_map(&parent.insert_container(key, LoroMap::new())?, fields)?;
            } else {
                parent.insert(key, LoroValue::from(row.clone()))?;
            }
        }
    }
    doc.commit();
    Ok(doc)
}
