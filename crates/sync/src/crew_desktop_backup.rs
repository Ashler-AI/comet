//! Offline desktop source receipts and private reconciliation; never authentication or a writer fence.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use loro::{ExportMode, IdSpan, LoroDoc, ToJson};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zeron_proto::CollaborationPrincipal;

use crate::crew_import::CrewImportError;

const EDGE: &str = "https://comet-staging.internal.ashler.com";
const RESOURCE: &str = "https://scaffold-staging.internal.ashler.com";
const STAGING: &str = "ashler-staging";
const EVIDENCE: &str = "source-evidence/desktop";
const DATABASE: &str = "source-evidence/desktop/docs.sqlite3";

fn unsafe_data(message: impl Into<String>) -> CrewImportError {
    CrewImportError::Unsafe(message.into())
}

/// Original authenticated frozen-room receipt, distinct from the bulk job ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopDocumentWitness {
    pub migration_id: String,
    pub sha256: String,
}

/// Supplied only after fresh identity verification. No credentials are serialized.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopSourceContext {
    pub migration_id: String,
    pub owner_subject: String,
    pub project_id: String,
    pub deployment_id: String,
    pub source_origin: String,
    pub source_resource: String,
    pub target_origin: String,
    pub target_resource: String,
    pub source_witnesses: BTreeMap<String, DesktopDocumentWitness>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopSourceProof {
    pub migration_id: String,
    pub owner_subject: String,
    pub project_id: String,
    pub deployment_id: String,
    pub source_origin: String,
    pub source_resource: String,
    pub target_origin: String,
    pub target_resource: String,
    pub source_database_sha256: String,
    pub source_files: BTreeMap<String, String>,
    /// Exact original SQLite receipts, including local-only documents and aliases.
    pub document_scopes: BTreeMap<String, Value>,
    pub source_witnesses: BTreeMap<String, DesktopDocumentWitness>,
    /// Generic bootstrap-only roots retained as original SQLite evidence, never rooms.
    pub inert_bootstrap_snapshots: BTreeMap<String, String>,
}

impl DesktopSourceContext {
    pub fn validate(&self, principal: &CollaborationPrincipal) -> Result<(), CrewImportError> {
        if self.migration_id.trim().is_empty()
            || self.owner_subject.trim().is_empty()
            || self.owner_subject != principal.subject
            || self.project_id != principal.project_id
            || self.project_id != STAGING
            || self.deployment_id != STAGING
            || principal.deployment_id.as_deref() != Some(STAGING)
            || principal.session_id.is_some()
            || self.source_origin != EDGE
            || self.target_origin != EDGE
            || self.source_resource != RESOURCE
            || self.target_resource != RESOURCE
        {
            return Err(unsafe_data(
                "desktop source requires exact freshly verified canonical staging source and target",
            ));
        }
        Ok(())
    }
}

impl DesktopSourceProof {
    /// Returns only an explicit original receipt ID, never the bulk job ID or
    /// an ID inferred from untrusted raw JSON. Local documents have no receipt.
    pub fn source_migration_id(&self, doc_id: &str) -> Result<&str, CrewImportError> {
        let path = raw_path(doc_id)
            .ok_or_else(|| unsafe_data("local document has no frozen source migration"))?;
        let witness = self
            .source_witnesses
            .get(doc_id)
            .ok_or_else(|| unsafe_data("missing explicit source migration witness"))?;
        if witness.migration_id.trim().is_empty()
            || witness.sha256.len() != 64
            || !witness
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.source_files.get(&path) != Some(&witness.sha256)
        {
            return Err(unsafe_data(
                "source migration witness differs from exact inventoried raw bytes",
            ));
        }
        Ok(&witness.migration_id)
    }

    fn validate_witness_inventory(&self) -> Result<(), CrewImportError> {
        let expected: BTreeSet<&str> = self
            .document_scopes
            .keys()
            .filter(|id| {
                !self.inert_bootstrap_snapshots.contains_key(*id) && raw_path(id).is_some()
            })
            .map(String::as_str)
            .collect();
        if self
            .source_witnesses
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            != expected
        {
            return Err(unsafe_data(
                "source witnesses must cover exactly the original remote document inventory",
            ));
        }
        for id in expected {
            self.source_migration_id(id)?;
        }
        Ok(())
    }

    fn validate_identity(&self, principal: &CollaborationPrincipal) -> Result<(), CrewImportError> {
        if self.migration_id.trim().is_empty()
            || self.owner_subject.trim().is_empty()
            || self.owner_subject != principal.subject
            || self.project_id != principal.project_id
            || self.project_id != STAGING
            || self.deployment_id != STAGING
            || principal.deployment_id.as_deref() != Some(STAGING)
            || principal.session_id.is_some()
            || self.source_origin != EDGE
            || self.target_origin != EDGE
            || self.source_resource != RESOURCE
            || self.target_resource != RESOURCE
        {
            return Err(unsafe_data(
                "desktop proof differs from freshly verified canonical staging identity",
            ));
        }
        Ok(())
    }

    /// Call validate_evidence once before using these per-row checks. This authorizes
    /// only the target projection, never rewriting the retained source scope.
    pub fn validate_source_scope(
        &self,
        doc_id: &str,
        source_scope: &Value,
        principal: &CollaborationPrincipal,
    ) -> Result<(), CrewImportError> {
        self.validate_identity(principal)?;
        if self.inert_bootstrap_snapshots.contains_key(doc_id) {
            return Err(unsafe_data(
                "inert bootstrap evidence has no runtime source authority",
            ));
        }
        if self.document_scopes.get(doc_id) != Some(source_scope) {
            return Err(unsafe_data(
                "source scope differs from original desktop receipt",
            ));
        }
        checked_scope(doc_id, source_scope, principal)?;
        let canonical = crate::crew_import_aliases::canonical_execution_id(doc_id);
        if let Some(id) = canonical {
            let original = self
                .document_scopes
                .get(id)
                .ok_or_else(|| unsafe_data("self alias lacks canonical source scope"))?;
            checked_scope(id, original, principal)?;
            if original.get("deployment") != source_scope.get("deployment") {
                return Err(unsafe_data(
                    "self alias and canonical source deployment differ",
                ));
            }
        }
        if let Some(path) = raw_path(doc_id) {
            if !self.source_files.contains_key(&path) {
                return Err(unsafe_data(
                    "source scope lacks authoritative raw inventory",
                ));
            }
            self.source_migration_id(doc_id)?;
        }
        Ok(())
    }

    /// Rebind both original SQLite and raw exports to the manifest inventory and
    /// freshly verified principal. Hashes alone never establish authentication.
    pub fn validate_evidence(
        &self,
        backup: &Path,
        files: &BTreeMap<String, String>,
        principal: &CollaborationPrincipal,
    ) -> Result<(), CrewImportError> {
        self.validate_identity(principal)?;
        self.validate_witness_inventory()?;
        let expected: BTreeSet<String> = self
            .document_scopes
            .keys()
            .filter(|id| !self.inert_bootstrap_snapshots.contains_key(*id))
            .filter_map(|id| raw_path(id))
            .chain(std::iter::once(DATABASE.into()))
            .collect();
        if !self.document_scopes.contains_key("workspace3")
            || self.source_files.keys().cloned().collect::<BTreeSet<_>>() != expected
            || self.source_files.get(DATABASE) != Some(&self.source_database_sha256)
        {
            return Err(unsafe_data(
                "desktop source inventory is incomplete or has unexpected files",
            ));
        }
        for (path, digest) in &self.source_files {
            if files.get(path) != Some(digest) {
                return Err(unsafe_data(format!(
                    "desktop source evidence hash mismatch: {path}"
                )));
            }
        }
        let db = immutable_database(&backup.join(DATABASE))?;
        if read_scopes(&db)? != self.document_scopes {
            return Err(unsafe_data(
                "desktop proof relabels original document scopes",
            ));
        }
        if crate::crew_backup::hash(&backup.join(DATABASE))? != self.source_database_sha256 {
            return Err(unsafe_data(
                "original desktop database evidence hash mismatch",
            ));
        }
        let mut associated_ids: BTreeSet<String> = self.source_witnesses.keys().cloned().collect();
        for (relative, digest) in files
            .iter()
            .filter(|(path, _)| path.starts_with("native-histories/") && path.ends_with(".jsonl"))
        {
            if !Path::new(relative)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
                || crate::crew_backup::hash(&backup.join(relative))? != *digest
            {
                return Err(unsafe_data(
                    "native header evidence is not an exact inventoried copied file",
                ));
            }
            associated_ids.insert(crate::crew_import::native_history_id(
                &backup.join(relative),
            )?);
        }
        let classified = crate::crew_import_aliases::inert_bootstrap_snapshots_from_store(
            &db, &associated_ids,
        )?;
        if classified != self.inert_bootstrap_snapshots {
            return Err(unsafe_data(
                "bootstrap exclusions differ from independently classified original evidence",
            ));
        }
        for (id, scope) in &self.document_scopes {
            if self.inert_bootstrap_snapshots.contains_key(id) {
                continue;
            }
            self.validate_source_scope(id, scope, principal)?;
            if let Some(path) = raw_path(id) {
                let raw = read_bytes(&backup.join(&path))?;
                if format!("{:x}", Sha256::digest(&raw)) != self.source_files[&path] {
                    return Err(unsafe_data(format!(
                        "desktop raw source evidence hash mismatch: {path}"
                    )));
                }
                decode_source(id, scope, &raw, self.source_migration_id(id)?, principal)?;
            }
        }
        Ok(())
    }
}

fn checked_scope(
    id: &str,
    scope: &Value,
    principal: &CollaborationPrincipal,
) -> Result<(), CrewImportError> {
    let fields = scope
        .as_object()
        .ok_or_else(|| unsafe_data("invalid original document scope"))?;
    if fields.len() != 4
        || !["project", "principal", "deployment", "session"]
            .iter()
            .all(|key| fields.contains_key(*key))
        || scope["project"].as_str() != Some(principal.project_id.as_str())
    {
        return Err(unsafe_data(
            "original document project or scope schema mismatch",
        ));
    }
    if id == "workspace3" {
        // The cached actor is attribution, not ownership of the shared workspace.
        if scope["principal"]
            .as_str()
            .is_none_or(|actor| actor.trim().is_empty())
            || !(scope["session"].is_null() || scope["session"].as_str() == Some(id))
            || !(scope["deployment"].is_null() || scope["deployment"].as_str() == Some(STAGING))
        {
            return Err(unsafe_data("invalid project workspace source receipt"));
        }
        return Ok(());
    }
    let canonical = crate::crew_import_aliases::canonical_execution_id(id);
    let local = zeron_proto::is_local_history_chat(id);
    if (!zeron_proto::is_cloud_session_id(id) && !local && canonical.is_none())
        || scope["principal"].as_str() != Some(principal.subject.as_str())
        || !(scope["session"].as_str() == Some(id)
            || canonical.is_some_and(|id| scope["session"].as_str() == Some(id)))
        || !(scope["deployment"].is_null()
            || (!local && scope["deployment"].as_str() == Some(STAGING)))
    {
        return Err(unsafe_data(format!(
            "unapproved desktop source scope: {id}"
        )));
    }
    Ok(())
}

fn raw_path(id: &str) -> Option<String> {
    if id == "workspace3" {
        Some(format!("{EVIDENCE}/workspace.json"))
    } else if zeron_proto::is_cloud_session_id(id) {
        Some(format!("{EVIDENCE}/sessions/{id}.json"))
    } else {
        None
    }
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, CrewImportError> {
    let mut bytes = Vec::new();
    crate::crew_backup::file(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn immutable_database(path: &Path) -> Result<Connection, CrewImportError> {
    let uri = format!(
        "file:{}?immutable=1",
        percent_encoding::utf8_percent_encode(
            path.to_str()
                .ok_or_else(|| unsafe_data("non-UTF8 source database path"))?,
            percent_encoding::NON_ALPHANUMERIC
        )
    );
    Ok(Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?)
}

fn read_scopes(db: &Connection) -> Result<BTreeMap<String, Value>, CrewImportError> {
    let mut scopes = BTreeMap::new();
    for row in db
        .prepare("SELECT doc_id,scope FROM document_scopes")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
    {
        let (id, scope) = row?;
        if scopes.insert(id, serde_json::from_str(&scope)?).is_some() {
            return Err(unsafe_data("duplicate original source scope"));
        }
    }
    Ok(scopes)
}


fn decode_source(
    id: &str,
    scope: &Value,
    raw: &[u8],
    migration: &str,
    principal: &CollaborationPrincipal,
) -> Result<Vec<u8>, CrewImportError> {
    let (expected, generation) = if id == "workspace3" {
        let envelope: Value = serde_json::from_slice(raw)?;
        let actor = envelope
            .pointer("/scope/exportedBySubject")
            .and_then(Value::as_str)
            .filter(|actor| !actor.trim().is_empty())
            .ok_or_else(|| unsafe_data("workspace export lacks original actor attribution"))?;
        (
            json!({"projectId":principal.project_id,"workspaceId":format!("ws4/{}",principal.project_id),
            "authority":"project","exportedBySubject":actor}),
            "workspace",
        )
    } else {
        (
            json!({"ownerSubject":principal.subject,"projectId":principal.project_id,
            "deploymentId":scope["deployment"],"logicalSessionId":id}),
            "session",
        )
    };
    crate::crew_remote_backup::reconstruct_frozen_storage(raw, &expected, generation, migration)
}

fn load_complete_state(bytes: &[u8]) -> Result<LoroDoc, CrewImportError> {
    let doc = LoroDoc::new();
    let status = doc.import(bytes)?;
    if status.pending.is_some()
        || doc.is_detached()
        || !crate::crew_import_aliases::state_matches_oplog(&doc, &doc.oplog_vv())?
        || doc.oplog_vv().is_empty()
    {
        return Err(unsafe_data(
            "source requires complete nonempty causal state without pending operations",
        ));
    }
    Ok(doc)
}

fn complete(bytes: &[u8]) -> Result<LoroDoc, CrewImportError> {
    let doc = load_complete_state(bytes)?;
    if doc.is_shallow() {
        return Err(unsafe_data("source has no full retained history"));
    }
    Ok(doc)
}

fn hash_json_value(digest: &mut Sha256, mut value: Value) -> Result<(), CrewImportError> {
    fn canonicalize(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.sort_keys();
                for value in object.values_mut() {
                    canonicalize(value);
                }
            }
            Value::Array(array) => array.iter_mut().for_each(canonicalize),
            // Value equality treats signed floating zeroes as equal, but keeps
            // floating-point numbers distinct from integer numbers.
            Value::Number(number)
                if *number == serde_json::Number::from_f64(0.0).unwrap() =>
            {
                *number = serde_json::Number::from_f64(0.0).unwrap();
            }
            _ => {}
        }
    }
    canonicalize(&mut value);
    serde_json::to_writer(digest, &value)?;
    Ok(())
}

/// Hash the exact exported JsonChange sequence without retaining peer history.
/// JSON objects are self-delimiting; domain tags distinguish headers, operations
/// and the span end. Memory is bounded by one native operation and its JSON value.
fn overlap_digest(doc: &LoroDoc, span: IdSpan) -> Result<[u8; 32], CrewImportError> {
    let mut digest = Sha256::new();
    digest.update(b"crew-desktop-overlap-v1\0");
    let mut change_id = None;
    doc.with_oplog(|oplog| {
        oplog.change_store().try_visit_json_ops::<CrewImportError>(span, |meta, op| {
            if change_id != Some(meta.id) {
                change_id = Some(meta.id);
                // Match native JsonChange export, including its sorted deps
                // array. Do not use ChangeMeta equality: it omits most fields.
                let mut deps: Vec<_> = meta.deps.iter().collect();
                deps.sort_unstable();
                let header = loro::JsonChange {
                    id: meta.id,
                    timestamp: meta.timestamp,
                    deps,
                    lamport: meta.lamport,
                    msg: meta.message.as_deref().map(str::to_owned),
                    ops: Vec::new(),
                };
                digest.update(b"\0change\0");
                hash_json_value(&mut digest, serde_json::to_value(header)?)?;
            }
            digest.update(b"\0op\0");
            hash_json_value(&mut digest, serde_json::to_value(op)?)
        })
    })?;
    digest.update(b"\0end-span\0");
    Ok(digest.finalize().into())
}

/// Recover shallow sources only against full retained history, corroborating both
/// the boundary and latest state via checkout (shallow documents cannot fork_at).
pub(crate) fn merge(cache: &[u8], authoritative: &[u8]) -> Result<Vec<u8>, CrewImportError> {
    let left = load_complete_state(cache)?;
    let right = load_complete_state(authoritative)?;
    if left.is_shallow() {
        if right.is_shallow() {
            return Err(unsafe_data(
                "neither source retains the complete causal history",
            ));
        }
        return merge(authoritative, cache);
    }
    let left_version = left.oplog_vv();
    let right_version = right.oplog_vv();
    let retained_floor = right.shallow_since_vv().to_vv();
    if retained_floor
        .iter()
        .any(|(peer, end)| left_version.get(peer).copied().unwrap_or(0) < *end)
    {
        return Err(unsafe_data(
            "full source does not cover trimmed authoritative history",
        ));
    }
    // Compare only extant shared operations; trimmed identities must be covered
    // by the full source, never treated as zero-length authority or reinitialized.
    let mut shared = false;
    for (peer, end) in left_version.iter() {
        let start = retained_floor.get(peer).copied().unwrap_or(0);
        let overlap = (*end).min(right_version.get(peer).copied().unwrap_or(0));
        if overlap > start {
            shared = true;
            let span = IdSpan::new(*peer, start, overlap);
            if overlap_digest(&left, span)? != overlap_digest(&right, span)? {
                return Err(unsafe_data("conflicting source operation identities"));
            }
        }
    }
    if !shared && retained_floor.is_empty() {
        return Err(unsafe_data(
            "cache and authority have no common causal operations",
        ));
    }
    // A shallow snapshot cannot fill its own trimmed history. After checking all
    // overlap, import only the missing retained operations, never snapshot state.
    let updates = right
        .export(ExportMode::updates(&left_version))
        .map_err(|error| zeron_doc::DocError::Schema(error.to_string()))?;
    let status = left.import(&updates)?;
    if status.pending.is_some()
        || left.is_detached()
        || !crate::crew_import_aliases::state_matches_oplog(&left, &left.oplog_vv())?
        || left.is_shallow()
    {
        return Err(unsafe_data(
            "merged desktop source has unresolved or trimmed causal history",
        ));
    }
    if right.is_shallow() {
        // Check both the retained boundary state and latest source state against
        // causal projections of the full union. Equality is corroboration only:
        // all retained operations above have already been imported and checked.
        for frontier in [right.shallow_since_frontiers(), right.oplog_frontiers()] {
            right.checkout(&frontier)?;
            let restored = left.fork_at(&frontier)?;
            if right.state_vv() != restored.state_vv()
                || right.get_deep_value().to_json_value()
                    != restored.get_deep_value().to_json_value()
            {
                return Err(unsafe_data(
                    "trimmed source state conflicts with retained full causal history",
                ));
            }
        }
    } else {
        // Reversed import must yield the same state; no source wins by file order.
        right.import(cache)?;
        if left.oplog_vv() != right.oplog_vv()
            || left.get_deep_value().to_json_value() != right.get_deep_value().to_json_value()
        {
            return Err(unsafe_data("conflicting desktop source merge"));
        }
    }
    Ok(left
        .export(ExportMode::Snapshot)
        .map_err(|error| zeron_doc::DocError::Schema(error.to_string()))?)
}

pub struct PreparedDesktopDatabase {
    pub file: tempfile::NamedTempFile,
    pub proof: DesktopSourceProof,
    /// Bundle-relative evidence path -> original independent copied source file.
    pub evidence: BTreeMap<String, PathBuf>,
}

/// Reconcile only into a mode-0600 temporary database. Original DB/raw bytes are
/// retained for the exporter to inventory, never updated, normalized or relabeled.
pub fn prepare_desktop_database(
    database: &Path,
    storage_directory: &Path,
    temporary_parent: &Path,
    principal: &CollaborationPrincipal,
    context: &DesktopSourceContext,
    native_ids: &BTreeSet<String>,
) -> Result<PreparedDesktopDatabase, CrewImportError> {
    context.validate(principal)?;
    let original_hash = crate::crew_backup::hash(database)?;
    let wal = database.with_file_name(format!(
        "{}-wal",
        database
            .file_name()
            .ok_or_else(|| unsafe_data("missing source database filename"))?
            .to_string_lossy()
    ));
    if wal.exists() && crate::crew_backup::file(&wal)?.metadata()?.len() != 0 {
        return Err(unsafe_data(
            "desktop source database has an uncheckpointed WAL",
        ));
    }
    let original = immutable_database(database)?;
    let scopes = read_scopes(&original)?;
    if !scopes.contains_key("workspace3") {
        return Err(unsafe_data("desktop source has no workspace receipt"));
    }
    let mut associated_ids = native_ids.clone();
    associated_ids.extend(context.source_witnesses.keys().cloned());
    let inert_bootstrap_snapshots = crate::crew_import_aliases::inert_bootstrap_snapshots_from_store(
        &original, &associated_ids,
    )?;
    let mut evidence = BTreeMap::from([(DATABASE.into(), database.to_owned())]);
    for (id, scope) in &scopes {
        if inert_bootstrap_snapshots.contains_key(id) {
            continue;
        }
        checked_scope(id, scope, principal)?;
        if let Some(path) = raw_path(id) {
            let relative = path
                .strip_prefix(&format!("{EVIDENCE}/"))
                .expect("fixed evidence prefix");
            evidence.insert(path.clone(), storage_directory.join(relative));
        }
    }
    // Explicit input directory is an exact inventory, not a fallback search path.
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(storage_directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "sessions" && entry.file_type()?.is_dir() {
            for session in std::fs::read_dir(entry.path())? {
                let session = session?;
                if !session.file_type()?.is_file() {
                    return Err(unsafe_data("nonregular authoritative session export"));
                }
                actual.insert(format!(
                    "{EVIDENCE}/sessions/{}",
                    session
                        .file_name()
                        .to_str()
                        .ok_or_else(|| unsafe_data("non-UTF8 source file"))?
                ));
            }
        } else if name == "workspace.json" && entry.file_type()?.is_file() {
            actual.insert(format!("{EVIDENCE}/workspace.json"));
        } else {
            return Err(unsafe_data(
                "unexpected authoritative source directory entry",
            ));
        }
    }
    if actual
        != evidence
            .keys()
            .filter(|path| path.as_str() != DATABASE)
            .cloned()
            .collect()
    {
        return Err(unsafe_data(
            "missing or unexpected authoritative source exports",
        ));
    }
    let source_files = evidence
        .iter()
        .map(|(key, path)| Ok((key.clone(), crate::crew_backup::hash(path)?)))
        .collect::<Result<BTreeMap<_, _>, CrewImportError>>()?;
    if source_files.get(DATABASE) != Some(&original_hash) {
        return Err(unsafe_data(
            "desktop source database changed while reading scopes",
        ));
    }
    let proof = DesktopSourceProof {
        migration_id: context.migration_id.clone(),
        owner_subject: context.owner_subject.clone(),
        project_id: context.project_id.clone(),
        deployment_id: context.deployment_id.clone(),
        source_origin: context.source_origin.clone(),
        source_resource: context.source_resource.clone(),
        target_origin: context.target_origin.clone(),
        target_resource: context.target_resource.clone(),
        source_database_sha256: original_hash,
        source_files,
        document_scopes: scopes,
        source_witnesses: context.source_witnesses.clone(),
        inert_bootstrap_snapshots,
    };
    proof.validate_witness_inventory()?;
    for (id, scope) in &proof.document_scopes {
        if !proof.inert_bootstrap_snapshots.contains_key(id) {
            proof.validate_source_scope(id, scope, principal)?;
        }
    }
    let mut file = tempfile::NamedTempFile::new_in(temporary_parent)?;
    std::io::copy(&mut crate::crew_backup::file(database)?, file.as_file_mut())?;
    file.as_file().sync_all()?;
    let mut db = Connection::open(file.path())?;
    db.execute_batch("PRAGMA journal_mode=DELETE;")?;
    let tx = db.transaction()?;
    for row in original
        .prepare("SELECT doc_id,bytes FROM snapshots")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
    {
        let (id, bytes) = row?;
        if id.starts_with("recovery-original/") || proof.inert_bootstrap_snapshots.contains_key(&id)
        {
            continue;
        }
        let scope = proof
            .document_scopes
            .get(&id)
            .ok_or_else(|| unsafe_data(format!("snapshot lacks original scope: {id}")))?;
        if let Some(path) = raw_path(&id) {
            let raw = read_bytes(&evidence[&path])?;
            let authoritative =
                decode_source(&id, scope, &raw, proof.source_migration_id(&id)?, principal)?;
            let merged = merge(&bytes, &authoritative)?;
            tx.execute(
                "UPDATE snapshots SET bytes=?1 WHERE doc_id=?2",
                rusqlite::params![merged, id],
            )?;
        } else {
            complete(&bytes)?;
        }
    }
    for id in proof.document_scopes.keys() {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM snapshots WHERE doc_id=?1)",
            [id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(unsafe_data(format!(
                "source scope lacks cached snapshot: {id}"
            )));
        }
    }
    tx.commit()?;
    drop(db);
    file.as_file().sync_all()?;
    for (path, source) in &evidence {
        if crate::crew_backup::hash(source)? != proof.source_files[path] {
            return Err(unsafe_data("desktop source changed during preparation"));
        }
    }
    if wal.exists() && crate::crew_backup::file(&wal)?.metadata()?.len() != 0 {
        return Err(unsafe_data("desktop source writer resumed"));
    }
    Ok(PreparedDesktopDatabase {
        file,
        proof,
        evidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    const CHAT: &str = "11111111-1111-4111-8111-111111111111";

    fn principal() -> CollaborationPrincipal {
        CollaborationPrincipal {
            subject: "owner".into(),
            email: None,
            project_id: STAGING.into(),
            deployment_id: Some(STAGING.into()),
            session_id: None,
            capabilities: vec![],
            unknown: BTreeMap::new(),
        }
    }

    fn context() -> DesktopSourceContext {
        DesktopSourceContext {
            migration_id: "desktop-migration".into(),
            owner_subject: "owner".into(),
            project_id: STAGING.into(),
            deployment_id: STAGING.into(),
            source_origin: EDGE.into(),
            source_resource: RESOURCE.into(),
            target_origin: EDGE.into(),
            target_resource: RESOURCE.into(),
            source_witnesses: BTreeMap::new(),
        }
    }

    fn scope(id: &str) -> Value {
        json!({"project":STAGING,"principal":"owner","deployment":null,"session":id})
    }

    fn snapshot(doc: &LoroDoc) -> Vec<u8> {
        doc.export(ExportMode::Snapshot).unwrap()
    }

    fn frozen(doc: &LoroDoc, scope: Value, generation: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"schemaVersion":1,"format":"legacy-session-sql-v1",
            "sourceGeneration":generation,"migrationId":"desktop-migration","scope":scope,
            "frozenAt":"2026-10-21T00:00:00Z","meta":[],
            "blobs":[{"name":"snapshot","idx":0,"bytesBase64":base64::engine::general_purpose::STANDARD.encode(snapshot(doc))}],
            "updates":[]})).unwrap()
    }

    #[test]
    fn desktop_evidence_preserves_originals_and_binds_exact_staging_sources() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let raw = root.join("raw");
        std::fs::create_dir_all(raw.join("sessions")).unwrap();
        let database = root.join("original.sqlite3");
        let db = Connection::open(&database).unwrap();
        db.execute_batch("CREATE TABLE snapshots(doc_id TEXT PRIMARY KEY,bytes BLOB); CREATE TABLE document_scopes(doc_id TEXT PRIMARY KEY,scope TEXT);").unwrap();
        let workspace = LoroDoc::new();
        workspace
            .get_map("meta")
            .insert("schemaVersion", 3i64)
            .unwrap();
        workspace
            .get_map("chats")
            .insert("foreign-row", "retained-project-data")
            .unwrap();
        workspace.commit();
        let session = LoroDoc::new();
        session
            .get_map("meta")
            .insert("schemaVersion", 2i64)
            .unwrap();
        session.get_map("meta").insert("chatId", CHAT).unwrap();
        session.commit();
        let mut workspace_scope = scope("workspace3");
        workspace_scope["principal"] = json!("original-workspace-actor");
        for (id, doc, receipt) in [
            ("workspace3", &workspace, workspace_scope),
            (CHAT, &session, scope(CHAT)),
        ] {
            db.execute(
                "INSERT INTO snapshots VALUES (?1,?2)",
                rusqlite::params![id, snapshot(doc)],
            )
            .unwrap();
            db.execute(
                "INSERT INTO document_scopes VALUES (?1,?2)",
                rusqlite::params![id, receipt.to_string()],
            )
            .unwrap();
        }
        let orphan_id = "unreferenced-bootstrap-root";
        let orphan = LoroDoc::new();
        orphan.get_map("meta").insert("chatId", orphan_id).unwrap();
        orphan
            .get_map("meta")
            .insert("schemaVersion", 2i64)
            .unwrap();
        orphan.commit();
        let orphan_bytes = snapshot(&orphan);
        db.execute(
            "INSERT INTO snapshots VALUES (?1,?2)",
            rusqlite::params![orphan_id, &orphan_bytes],
        )
        .unwrap();
        drop(db);
        let original_bytes = std::fs::read(&database).unwrap();
        session
            .get_map("meta")
            .insert("authoritativeOnly", "retained")
            .unwrap();
        session.commit();
        let session_raw = frozen(
            &session,
            json!({"ownerSubject":"owner","projectId":STAGING,
            "deploymentId":null,"logicalSessionId":CHAT}),
            "session",
        );
        let mut session_envelope: Value = serde_json::from_slice(&session_raw).unwrap();
        session_envelope["migrationId"] = json!("retained-room-migration");
        let session_raw = serde_json::to_vec(&session_envelope).unwrap();
        std::fs::write(
            raw.join("sessions").join(format!("{CHAT}.json")),
            &session_raw,
        )
        .unwrap();
        std::fs::write(raw.join("workspace.json"), frozen(&workspace, json!({"projectId":STAGING,
            "workspaceId":format!("ws4/{STAGING}"),"authority":"project","exportedBySubject":"original-export-actor"}), "workspace")).unwrap();
        let mut context = context();
        for (id, path) in [
            ("workspace3", raw.join("workspace.json")),
            (CHAT, raw.join("sessions").join(format!("{CHAT}.json"))),
        ] {
            context.source_witnesses.insert(
                id.into(),
                DesktopDocumentWitness {
                    migration_id: if id == CHAT {
                        "retained-room-migration"
                    } else {
                        "desktop-migration"
                    }
                    .into(),
                    sha256: crate::crew_backup::hash(&path).unwrap(),
                },
            );
        }
        assert!(
            prepare_desktop_database(
                &database,
                &raw,
                &root,
                &principal(),
                &context,
                &BTreeSet::from([orphan_id.into()])
            )
            .is_err()
        );
        let prepared = prepare_desktop_database(
            &database,
            &raw,
            &root,
            &principal(),
            &context,
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(std::fs::read(&database).unwrap(), original_bytes);
        let merged = immutable_database(prepared.file.path()).unwrap();
        let merged_session: Vec<u8> = merged
            .query_row(
                "SELECT bytes FROM snapshots WHERE doc_id=?1",
                [CHAT],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            complete(&merged_session)
                .unwrap()
                .get_deep_value()
                .to_json_value(),
            session.get_deep_value().to_json_value()
        );
        assert_eq!(
            read_scopes(&merged).unwrap(),
            prepared.proof.document_scopes
        );
        let retained_orphan: Vec<u8> = merged
            .query_row(
                "SELECT bytes FROM snapshots WHERE doc_id=?1",
                [orphan_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_orphan, orphan_bytes);
        assert!(
            prepared
                .proof
                .inert_bootstrap_snapshots
                .contains_key(orphan_id)
        );
        assert!(!prepared.proof.source_witnesses.contains_key(orphan_id));
        assert!(prepared.proof.source_migration_id(orphan_id).is_err());
        let bundle = root.join("bundle");
        for (relative, source) in &prepared.evidence {
            let target = bundle.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(source, target).unwrap();
        }
        prepared
            .proof
            .validate_evidence(&bundle, &prepared.proof.source_files, &principal())
            .unwrap();
        let mut missing_exclusion = prepared.proof.clone();
        missing_exclusion.inert_bootstrap_snapshots.clear();
        assert!(
            missing_exclusion
                .validate_evidence(&bundle, &prepared.proof.source_files, &principal())
                .is_err()
        );
        let native_relative = "native-histories/orphan/session.jsonl";
        std::fs::create_dir_all(bundle.join("native-histories/orphan")).unwrap();
        std::fs::write(
            bundle.join(native_relative),
            format!("{}\n", json!({"type":"session","id":orphan_id})),
        )
        .unwrap();
        let mut with_native = prepared.proof.source_files.clone();
        with_native.insert(
            native_relative.into(),
            crate::crew_backup::hash(&bundle.join(native_relative)).unwrap(),
        );
        assert!(
            prepared
                .proof
                .validate_evidence(&bundle, &with_native, &principal())
                .is_err()
        );
        assert_eq!(prepared.proof.migration_id, "desktop-migration");
        assert_eq!(
            prepared.proof.source_migration_id(CHAT).unwrap(),
            "retained-room-migration"
        );
        assert_eq!(
            prepared.proof.source_migration_id("workspace3").unwrap(),
            "desktop-migration"
        );
        for scenario in ["missing", "digest", "migration", "extra"] {
            let mut changed = prepared.proof.clone();
            match scenario {
                "missing" => {
                    changed.source_witnesses.remove(CHAT);
                }
                "digest" => changed.source_witnesses.get_mut(CHAT).unwrap().sha256 = "0".repeat(64),
                "migration" => {
                    changed.source_witnesses.get_mut(CHAT).unwrap().migration_id =
                        "desktop-migration".into()
                }
                "extra" => {
                    changed.source_witnesses.insert(
                        "local-chat-other".into(),
                        changed.source_witnesses[CHAT].clone(),
                    );
                }
                _ => unreachable!(),
            }
            assert!(
                changed
                    .validate_evidence(&bundle, &prepared.proof.source_files, &principal())
                    .is_err(),
                "accepted {scenario} witness"
            );
        }
        for field in [
            "sourceOrigin",
            "targetOrigin",
            "sourceResource",
            "targetResource",
            "ownerSubject",
            "deploymentId",
            "projectId",
        ] {
            let mut value = serde_json::to_value(&prepared.proof).unwrap();
            value[field] = json!("not-approved");
            let changed: DesktopSourceProof = serde_json::from_value(value).unwrap();
            assert!(
                changed
                    .validate_source_scope(CHAT, &scope(CHAT), &principal())
                    .is_err(),
                "accepted {field}"
            );
        }
        let mut wrong_raw: Value = serde_json::from_slice(&session_raw).unwrap();
        wrong_raw["scope"]["deploymentId"] = json!(STAGING);
        assert!(
            decode_source(
                CHAT,
                &scope(CHAT),
                &serde_json::to_vec(&wrong_raw).unwrap(),
                "retained-room-migration",
                &principal()
            )
            .is_err()
        );
        let mut scoped = scope(CHAT);
        scoped["deployment"] = json!(STAGING);
        assert!(
            decode_source(
                CHAT,
                &scoped,
                &session_raw,
                "retained-room-migration",
                &principal()
            )
            .is_err()
        );
        std::fs::write(bundle.join(raw_path(CHAT).unwrap()), b"changed").unwrap();
        assert!(
            prepared
                .proof
                .validate_evidence(&bundle, &prepared.proof.source_files, &principal())
                .is_err()
        );
    }

    #[test]
    fn source_identifiers_and_local_history_never_gain_remote_authority() {
        for id in [
            "ses_short",
            "rcs_short",
            "short",
            "11111111-1111-4111-8111-111111111111::session::different",
        ] {
            assert!(checked_scope(id, &scope(id), &principal()).is_err());
            assert!(raw_path(id).is_none());
        }
        let alias = format!("{CHAT}::session::{CHAT}");
        assert!(checked_scope(&alias, &scope(&alias), &principal()).is_ok());
        assert!(raw_path(&alias).is_none());
        let local = "local-chat-desktop";
        assert!(checked_scope(local, &scope(local), &principal()).is_ok());
        assert!(raw_path(local).is_none());
        let mut scoped_local = scope(local);
        scoped_local["deployment"] = json!(STAGING);
        assert!(checked_scope(local, &scoped_local, &principal()).is_err());
    }

    #[test]
    fn reconciliation_rejects_operation_collisions_and_missing_causal_history() {
        let left = LoroDoc::new();
        let right = LoroDoc::new();
        for doc in [&left, &right] {
            doc.set_peer_id(7).unwrap();
        }
        left.get_map("meta").insert("value", "left").unwrap();
        right.get_map("meta").insert("value", "right").unwrap();
        left.commit();
        right.commit();
        assert!(merge(&snapshot(&left), &snapshot(&right)).is_err());
        let unrelated = LoroDoc::new();
        unrelated.set_peer_id(8).unwrap();
        unrelated
            .get_map("meta")
            .insert("value", "other-history")
            .unwrap();
        unrelated.commit();
        assert!(merge(&snapshot(&left), &snapshot(&unrelated)).is_err());
        assert!(merge(&snapshot(&left), &snapshot(&LoroDoc::new())).is_err());
        let before = left.oplog_vv();
        left.get_map("meta")
            .insert("later", "requires-prior-op")
            .unwrap();
        left.commit();
        let incomplete = left.export(ExportMode::updates(&before)).unwrap();
        assert!(complete(&incomplete).is_err());
    }

    #[test]
    fn overlap_hash_matches_value_equality_and_native_partial_change_export() {
        let value_digest = |value| {
            let mut digest = Sha256::new();
            hash_json_value(&mut digest, value).unwrap();
            <[u8; 32]>::from(digest.finalize())
        };
        let first: Value = serde_json::from_str(r#"{"b":[{"y":-0.0,"x":1}],"a":null}"#).unwrap();
        let reordered: Value = serde_json::from_str(r#"{"a":null,"b":[{"x":1,"y":0.0}]}"#).unwrap();
        assert_eq!(first, reordered);
        assert_eq!(value_digest(first), value_digest(reordered));
        assert_ne!(value_digest(json!([1, 2])), value_digest(json!([2, 1])));
        assert_ne!(value_digest(json!(0)), value_digest(json!(0.0)));

        let make_doc = |timestamp, message: Option<&str>, split| {
            let doc = LoroDoc::new();
            doc.set_peer_id(7).unwrap();
            doc.set_change_merge_interval(-1);
            doc.set_next_commit_timestamp(timestamp);
            if let Some(message) = message {
                doc.set_next_commit_message(message);
            }
            doc.get_text("text").insert(0, "a").unwrap();
            if split {
                doc.commit();
                doc.set_next_commit_timestamp(timestamp);
                if let Some(message) = message {
                    doc.set_next_commit_message(message);
                }
            }
            doc.get_text("text").insert(1, "bc").unwrap();
            doc.commit();
            doc
        };
        let source = make_doc(123, Some("commit"), false);
        let same = complete(&snapshot(&source)).unwrap();
        let full = IdSpan::new(7, 0, 3);
        assert_eq!(overlap_digest(&source, full).unwrap(), overlap_digest(&same, full).unwrap());
        for changed in [
            make_doc(124, Some("commit"), false),
            make_doc(123, Some("different"), false),
            make_doc(123, None, false),
            make_doc(123, Some(""), false),
            make_doc(123, Some("commit"), true),
        ] {
            assert_ne!(
                serde_json::to_value(source.export_json_in_id_span(full)).unwrap(),
                serde_json::to_value(changed.export_json_in_id_span(full)).unwrap(),
            );
            assert_ne!(overlap_digest(&source, full).unwrap(), overlap_digest(&changed, full).unwrap());
        }
        let dependency = LoroDoc::new();
        dependency.set_peer_id(8).unwrap();
        dependency.get_map("meta").insert("independent", true).unwrap();
        dependency.commit();
        let joined = complete(&snapshot(&source)).unwrap();
        joined.import(&snapshot(&dependency)).unwrap();
        joined.set_peer_id(9).unwrap();
        joined.get_map("meta").insert("joined", true).unwrap();
        joined.commit();
        let split = make_doc(123, Some("commit"), true);
        // Reference the old exporter for whole and sliced changes, including
        // adjusted ID/lamport, multi-peer deps order and operation boundaries.
        for (doc, peer, end) in [(&source, 7, 3), (&split, 7, 3), (&joined, 9, 1)] {
            for span in [IdSpan::new(peer, 0, end), IdSpan::new(peer, 0, 1), IdSpan::new(peer, end - 1, end)] {
                let mut expected = Sha256::new();
                expected.update(b"crew-desktop-overlap-v1\0");
                for mut change in doc.export_json_in_id_span(span) {
                    let ops = std::mem::take(&mut change.ops);
                    expected.update(b"\0change\0");
                    hash_json_value(&mut expected, serde_json::to_value(change).unwrap()).unwrap();
                    for op in ops {
                        expected.update(b"\0op\0");
                        hash_json_value(&mut expected, serde_json::to_value(op).unwrap()).unwrap();
                    }
                }
                expected.update(b"\0end-span\0");
                assert_eq!(overlap_digest(doc, span).unwrap(), <[u8; 32]>::from(expected.finalize()));
            }
        }
        let prefix = LoroDoc::new();
        prefix.import(&source.export(ExportMode::updates_in_range(vec![IdSpan::new(7, 0, 1)])).unwrap()).unwrap();
        let merged = complete(&merge(&snapshot(&prefix), &snapshot(&source)).unwrap()).unwrap();
        assert_eq!(merged.oplog_vv(), source.oplog_vv());
        assert_eq!(merged.get_deep_value(), source.get_deep_value());
        assert!(merge(&snapshot(&source), &snapshot(&make_doc(124, Some("commit"), false))).is_err());
    }

    #[test]
    fn shallow_authority_recovers_only_with_full_retained_floor_coverage() {
        let origin = LoroDoc::new();
        origin.set_peer_id(7).unwrap();
        origin
            .get_map("meta")
            .insert("first", "preserved-history")
            .unwrap();
        origin.commit();
        let too_old = snapshot(&origin);
        origin
            .get_map("meta")
            .insert("second", "preserved-history")
            .unwrap();
        origin.get_map("meta").insert("third", "boundary").unwrap();
        origin.commit();
        let full_base = snapshot(&origin);
        let shallow_bytes = origin
            .export(ExportMode::shallow_snapshot(&origin.oplog_frontiers()))
            .unwrap();
        let edge = load_complete_state(&shallow_bytes).unwrap();
        assert!(edge.is_shallow());
        edge.set_peer_id(18).unwrap();
        edge.get_map("meta").insert("edgeOnly", "retained").unwrap();
        edge.commit();
        let host = complete(&full_base).unwrap();
        host.set_peer_id(17).unwrap();
        host.get_map("meta").insert("hostOnly", "retained").unwrap();
        host.commit();
        let host_bytes = snapshot(&host);
        let edge_bytes = snapshot(&edge);
        let merged = complete(&merge(&host_bytes, &edge_bytes).unwrap()).unwrap();
        assert!(!merged.is_shallow());
        let value = merged.get_deep_value().to_json_value();
        for key in ["first", "second", "third", "hostOnly", "edgeOnly"] {
            assert!(value["meta"].get(key).is_some(), "lost {key}");
        }
        assert_eq!(
            merged
                .fork_at(&origin.oplog_frontiers())
                .unwrap()
                .get_deep_value()
                .to_json_value(),
            origin.get_deep_value().to_json_value()
        );
        assert_eq!(
            complete(&merge(&edge_bytes, &host_bytes).unwrap())
                .unwrap()
                .oplog_vv(),
            merged.oplog_vv()
        );
        assert!(merge(&too_old, &edge_bytes).is_err());
        assert!(merge(&shallow_bytes, &edge_bytes).is_err());
    }
}
