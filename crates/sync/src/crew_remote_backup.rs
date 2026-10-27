//! Offline binding checks for a provider-fenced, independently copied remote profile.
//! This context is NOT authentication or a stop receipt. Only the authenticated
//! provider coordinator may derive it and assert the external writer fence.
use crate::crew_import::{CrewBackupManifest, CrewImportError};
use loro::{ExportMode, LoroDoc, ToJson};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use zeron_proto::{
    AgentSessionSource, CollaborationPrincipal, CollaborationScope, CrewChatMetadata, HarnessId,
    PublicationRecord, PublicationValue, SessionEnvironment, SessionEnvironmentSource,
};

fn unsafe_data(message: &str) -> CrewImportError {
    CrewImportError::Unsafe(message.into())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteBackupContext {
    pub migration_id: String,
    pub owner_subject: String,
    pub project_id: String,
    pub deployment_id: String,
    pub logical_session_id: String,
    pub sandbox_id: String,
    pub device_id: String,
    pub lifecycle_epoch: u64,
    pub source_generation: String,
    pub writers_stopped: bool,
}

impl RemoteBackupContext {
    pub fn validate(&self) -> Result<(), CrewImportError> {
        if [
            &self.migration_id,
            &self.owner_subject,
            &self.project_id,
            &self.deployment_id,
            &self.sandbox_id,
            &self.device_id,
        ]
        .iter()
        .any(|s| s.trim().is_empty())
            || !zeron_proto::is_cloud_session_id(&self.logical_session_id)
            || self.source_generation != "session"
            || !self.writers_stopped
        {
            return Err(unsafe_data(
                "exact remote identity, legacy session generation and externally stopped writers required",
            ));
        }
        Ok(())
    }

    pub fn principal(&self) -> Result<CollaborationPrincipal, CrewImportError> {
        self.validate()?;
        Ok(CollaborationPrincipal {
            subject: self.owner_subject.clone(),
            email: None,
            project_id: self.project_id.clone(),
            deployment_id: Some(self.deployment_id.clone()),
            session_id: None,
            capabilities: vec![],
            unknown: BTreeMap::new(),
        })
    }

    fn scope(&self) -> CollaborationScope {
        CollaborationScope {
            project_id: self.project_id.clone(),
            deployment_id: Some(self.deployment_id.clone()),
            session_id: Some(self.logical_session_id.clone()),
            unknown: BTreeMap::new(),
        }
    }

    pub(crate) fn environment(
        &self,
        environment: &SessionEnvironment,
    ) -> Result<(), CrewImportError> {
        if environment.owner_principal != self.owner_subject
            || environment.scope != self.scope()
            || !matches!(&environment.source, SessionEnvironmentSource::Scaffold { sandbox_id, lifecycle_epoch, .. }
                if sandbox_id == &self.sandbox_id && *lifecycle_epoch == Some(self.lifecycle_epoch))
        {
            return Err(unsafe_data(
                "remote environment differs from fenced owner/scope/sandbox/epoch",
            ));
        }
        Ok(())
    }

    pub(crate) fn document(
        &self,
        id: &str,
        device: &str,
        row: &Value,
        doc: &Value,
    ) -> Result<CrewChatMetadata, CrewImportError> {
        self.document_identity(id, device, row, doc, HarnessId::Omp, true)
    }

    pub(crate) fn original_epoch_document(
        &self,
        id: &str,
        device: &str,
        row: &Value,
        doc: &Value,
        harness: HarnessId,
    ) -> Result<CrewChatMetadata, CrewImportError> {
        self.document_identity(id, device, row, doc, harness, false)
    }

    fn document_identity(
        &self,
        id: &str,
        device: &str,
        row: &Value,
        doc: &Value,
        harness: HarnessId,
        require_native: bool,
    ) -> Result<CrewChatMetadata, CrewImportError> {
        if id != self.logical_session_id || device != self.device_id {
            return Err(unsafe_data(
                "copied remote session or host differs from fenced identity",
            ));
        }
        if let Some(value) = row.get("environment").filter(|value| !value.is_null()) {
            self.environment(&serde_json::from_value(value.clone())?)?;
        }
        let mut metadata = None;
        if let Some(value) = row.get("crew").filter(|value| !value.is_null()) {
            let value: CrewChatMetadata = serde_json::from_value(value.clone())?;
            self.chat(&value)?;
            metadata = Some(value);
        }
        // Shared publications remain byte-exact historical evidence, not grants
        // to import the other contributor's session or execute their commands.
        let native_id = row
            .get("harnessSessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        if require_native && native_id.is_none() {
            return Err(unsafe_data("remote native journal identity is missing"));
        }
        let records = doc
            .get("agentSessions")
            .map(|value| {
                value
                    .as_object()
                    .ok_or_else(|| unsafe_data("invalid remote agentSessions"))
            })
            .transpose()?;
        if let Some(records) = records {
            for (key, value) in records {
                let record: PublicationRecord = serde_json::from_value(value.clone())?;
                if !matches!(&record.value, PublicationValue::AgentSession(session) if &session.session_id == key)
                {
                    return Err(unsafe_data("remote agent session register key mismatch"));
                }
                self.publication(record, native_id, harness, &mut metadata)?;
            }
        }
        if let Some(records) = doc.get("publications") {
            for value in records
                .as_array()
                .ok_or_else(|| unsafe_data("invalid remote publications"))?
            {
                self.publication(
                    serde_json::from_value(
                        value
                            .get("record")
                            .ok_or_else(|| unsafe_data("remote publication missing record"))?
                            .clone(),
                    )?,
                    native_id,
                    harness,
                    &mut metadata,
                )?;
            }
        }
        metadata.ok_or_else(|| unsafe_data("no persisted exact remote owner/environment evidence"))
    }

    fn publication(
        &self,
        record: PublicationRecord,
        native_id: Option<&str>,
        harness: HarnessId,
        metadata: &mut Option<CrewChatMetadata>,
    ) -> Result<(), CrewImportError> {
        zeron_doc::validate_publication(&record)?;
        if let PublicationValue::AgentSession(session) = record.value {
            if session.chat_id != self.logical_session_id {
                return Err(unsafe_data("publication belongs to another thread"));
            }
            if session.session_id != self.logical_session_id {
                return Ok(());
            }
            if session.owner_subject != self.owner_subject
                || session.owner_device_id != self.device_id
                || session.source != AgentSessionSource::Scaffold
                || session.harness != Some(harness)
                || session.harness_session_id.as_deref() != native_id
            {
                return Err(unsafe_data(
                    "remote owner publication conflicts with fenced host",
                ));
            }
            let environment = session
                .environment
                .ok_or_else(|| unsafe_data("remote owner publication lacks environment"))?;
            self.environment(&environment)?;
            if metadata.is_none() {
                *metadata = Some(CrewChatMetadata {
                    owner_subject: self.owner_subject.clone(),
                    scope: self.scope(),
                    session_id: self.logical_session_id.clone(),
                    environment: Some(environment),
                    agent_route: None,
                    original_epoch_history: None,
                });
            }
        }
        Ok(())
    }

    pub fn chat(&self, chat: &CrewChatMetadata) -> Result<(), CrewImportError> {
        chat.validate().map_err(unsafe_data)?;
        if chat.owner_subject != self.owner_subject
            || chat.scope != self.scope()
            || chat.session_id != self.logical_session_id
        {
            return Err(unsafe_data(
                "backup chat differs from exact remote identity",
            ));
        }
        self.environment(
            chat.environment
                .as_ref()
                .ok_or_else(|| unsafe_data("remote backup has no persisted environment"))?,
        )
    }

    pub fn manifest(&self, manifest: &CrewBackupManifest) -> Result<(), CrewImportError> {
        self.validate()?;
        if !manifest.writer_stopped
            || manifest.owner_subject != self.owner_subject
            || manifest.project_id != self.project_id
            || manifest.deployment_id.as_deref() != Some(&self.deployment_id)
            || manifest.chats.len() != 1
        {
            return Err(unsafe_data("backup differs from exact remote context"));
        }
        self.chat(
            manifest
                .chats
                .get(&self.logical_session_id)
                .ok_or_else(|| unsafe_data("remote backup session missing"))?,
        )
    }
}

pub fn export_remote_crew_backup(
    source_profile_root: &Path,
    source_backup_root: &Path,
    database_relative: &Path,
    destination: &Path,
    native_history_roots: &[PathBuf],
    authoritative_storage: &Path,
    attachment_root: Option<&Path>,
    context: &RemoteBackupContext,
) -> Result<CrewBackupManifest, CrewImportError> {
    let principal = context.principal()?;
    match crate::crew_backup::export_crew_backup(
        source_profile_root,
        source_backup_root,
        database_relative,
        destination,
        native_history_roots,
        &principal,
        Some((context, authoritative_storage)),
        None,
        attachment_root,
        false,
        None,
    )? {
        crate::crew_backup::BackupOutput::Backup(manifest) => Ok(manifest),
        crate::crew_backup::BackupOutput::Inventory(_) => {
            Err(unsafe_data("unexpected export inventory result"))
        }
    }
}

pub fn inventory_remote_crew_backup(
    source_profile_root: &Path,
    source_backup_root: &Path,
    database_relative: &Path,
    destination: &Path,
    native_history_roots: &[PathBuf],
    authoritative_storage: &Path,
    attachment_root: Option<&Path>,
    context: &RemoteBackupContext,
) -> Result<crate::crew_backup_assets::AttachmentInventory, CrewImportError> {
    let principal = context.principal()?;
    match crate::crew_backup::export_crew_backup(
        source_profile_root,
        source_backup_root,
        database_relative,
        destination,
        native_history_roots,
        &principal,
        Some((context, authoritative_storage)),
        None,
        attachment_root,
        true,
        None,
    )? {
        crate::crew_backup::BackupOutput::Inventory(inventory) => Ok(inventory),
        crate::crew_backup::BackupOutput::Backup(_) => {
            Err(unsafe_data("inventory unexpectedly exported a backup"))
        }
    }
}

pub(crate) struct PreparedRemoteDatabase {
    pub file: tempfile::NamedTempFile,
    pub host_sha256: String,
    pub edge_sha256: String,
}

#[cfg(test)]
fn import_complete(doc: &LoroDoc, bytes: &[u8]) -> Result<(), CrewImportError> {
    let status = doc.import(bytes)?;
    if status.pending.is_some() || doc.is_detached() || !crate::crew_import_aliases::state_matches_oplog(doc, &doc.oplog_vv())? {
        return Err(unsafe_data(
            "authoritative reconciliation has unresolved causal operations",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrozenStorage {
    schema_version: u32,
    format: String,
    source_generation: String,
    migration_id: String,
    scope: Value,
    frozen_at: String,
    meta: Vec<FrozenMeta>,
    blobs: Vec<FrozenBlob>,
    updates: Vec<FrozenUpdate>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenMeta {
    key: String,
    value: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrozenBlob {
    name: String,
    idx: u64,
    bytes_base64: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrozenUpdate {
    seq: u64,
    received_at: i64,
    bytes_base64: String,
}

fn reconstruct_storage(
    bytes: &[u8],
    context: &RemoteBackupContext,
) -> Result<Vec<u8>, CrewImportError> {
    context.validate()?;
    reconstruct_frozen_storage(
        bytes,
        &serde_json::json!({
            "ownerSubject":context.owner_subject,"projectId":context.project_id,
            "deploymentId":context.deployment_id,"logicalSessionId":context.logical_session_id,
        }),
        "session",
        &context.migration_id,
    )
}

/// Decode exact frozen bytes against a separately authenticated retained source
/// receipt. This helper never authorizes a caller, rewrites scope, or imports a
/// shared workspace as owned. Desktop callers supply their actual null scope.
pub fn reconstruct_frozen_storage(
    bytes: &[u8],
    expected_scope: &Value,
    source_generation: &str,
    migration_id: &str,
) -> Result<Vec<u8>, CrewImportError> {
    let fields = expected_scope
        .as_object()
        .ok_or_else(|| unsafe_data("expected source scope must be an object"))?;
    let required: &[&str] = match source_generation {
        "session" => &["ownerSubject", "projectId", "logicalSessionId"],
        "workspace" => &["projectId", "workspaceId", "authority", "exportedBySubject"],
        _ => return Err(unsafe_data("unknown frozen source generation")),
    };
    if fields.len() != 4
        || migration_id.trim().is_empty()
        || required.iter().any(|key| {
            fields
                .get(*key)
                .and_then(Value::as_str)
                .is_none_or(|value| value.trim().is_empty())
        })
        || (source_generation == "session"
            && (!zeron_proto::is_cloud_session_id(
                expected_scope["logicalSessionId"].as_str().unwrap(),
            ) || !fields.contains_key("deploymentId")
                || (!expected_scope["deploymentId"].is_null()
                    && expected_scope["deploymentId"]
                        .as_str()
                        .is_none_or(|value| value.trim().is_empty()))))
        || (source_generation == "workspace"
            && expected_scope["authority"].as_str() != Some("project"))
    {
        return Err(unsafe_data(
            "incomplete or unsupported expected frozen source scope",
        ));
    }
    use base64::Engine;
    let storage: FrozenStorage = serde_json::from_slice(bytes)?;
    if storage.schema_version != 1
        || storage.format != "legacy-session-sql-v1"
        || storage.source_generation != source_generation
        || storage.migration_id != migration_id
        || &storage.scope != expected_scope
        || chrono::DateTime::parse_from_rfc3339(&storage.frozen_at).is_err()
    {
        return Err(unsafe_data(
            "frozen Edge storage differs from the server-selected migration and room scope",
        ));
    }
    let mut previous_key = None;
    for row in &storage.meta {
        if previous_key.is_some_and(|previous: &str| previous >= row.key.as_str()) {
            return Err(unsafe_data(
                "frozen metadata keys are duplicate or unordered",
            ));
        }
        previous_key = Some(row.key.as_str());
        let expected = match row.key.as_str() {
            "chatId" => expected_scope
                .get(if source_generation == "session" {
                    "logicalSessionId"
                } else {
                    "workspaceId"
                })
                .and_then(Value::as_str),
            "scopedSessionId" => expected_scope
                .get("logicalSessionId")
                .and_then(Value::as_str),
            "owner" => expected_scope.get("projectId").and_then(Value::as_str),
            "ownerUserId" => expected_scope.get("ownerSubject").and_then(Value::as_str),
            "deploymentId" => Some(
                expected_scope
                    .get("deploymentId")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            ),
            _ => None,
        };
        if expected.is_some_and(|expected| expected != row.value) {
            return Err(unsafe_data("frozen metadata conflicts with room authority"));
        }
    }
    let decoder = base64::engine::general_purpose::STANDARD;
    let mut snapshot = Vec::new();
    let mut previous_name = None;
    let mut next_index = 0;
    for row in &storage.blobs {
        if previous_name != Some(row.name.as_str()) {
            if row.name.is_empty()
                || previous_name.is_some_and(|previous: &str| previous >= row.name.as_str())
            {
                return Err(unsafe_data("frozen blob names are unordered"));
            }
            previous_name = Some(row.name.as_str());
            next_index = 0;
        }
        if row.idx != next_index {
            return Err(unsafe_data(
                "frozen blob chunk is missing, duplicated or unordered",
            ));
        }
        next_index += 1;
        if row.name == "snapshot" {
            decoder
                .decode_vec(&row.bytes_base64, &mut snapshot)
                .map_err(|_| unsafe_data("invalid frozen snapshot base64"))?;
        }
        // Non-Loro sidecars (tail/diff/etc.) remain byte-exact in the raw envelope.
    }
    let doc = LoroDoc::new();
    let mut pending = BTreeMap::<u64, i32>::new();
    let mut replay = |bytes: &[u8]| -> Result<(), CrewImportError> {
        if let Some(ranges) = doc.import(bytes)?.pending {
            for (peer, (_, end)) in ranges.iter() {
                pending
                    .entry(*peer)
                    .and_modify(|known| *known = (*known).max(*end))
                    .or_insert(*end);
            }
        }
        Ok(())
    };
    if !snapshot.is_empty() {
        replay(&snapshot)?;
    }
    let mut previous_seq = 0;
    for row in &storage.updates {
        if row.seq <= previous_seq || row.received_at < 0 {
            return Err(unsafe_data("frozen updates are duplicated or unordered"));
        }
        previous_seq = row.seq;
        let bytes = decoder
            .decode(&row.bytes_base64)
            .map_err(|_| unsafe_data("invalid frozen update base64"))?;
        replay(&bytes)?;
    }
    let version = doc.oplog_vv();
    if pending
        .iter()
        .any(|(peer, end)| version.get(peer).copied().unwrap_or(0) < *end)
        || doc.is_detached()
        || !crate::crew_import_aliases::state_matches_oplog(&doc, &version)?
    {
        return Err(unsafe_data(
            "frozen storage contains unresolved accepted causal operations",
        ));
    }
    let value = doc.get_deep_value().to_json_value();
    let schema = value.pointer("/meta/schemaVersion").and_then(Value::as_u64);
    if (source_generation == "session"
        && (value.pointer("/meta/chatId").and_then(Value::as_str)
            != expected_scope["logicalSessionId"].as_str()
            || !matches!(schema, Some(1 | 2))))
        || (source_generation == "workspace" && schema != Some(3))
    {
        return Err(unsafe_data(
            "frozen document identity or schema differs from its source authority",
        ));
    }
    doc.export(ExportMode::Snapshot)
        .map_err(|_| unsafe_data("cannot encode reconstructed frozen state"))
}

/// Merge captured bytes, NOT a live-write barrier. The coordinator must already
/// fence every old Edge writer as well as the sandbox; a snapshot file or two
/// equal reads cannot establish that precondition. Originals remain inventoried.
pub(crate) fn prepare_authoritative_database(
    database: &Path,
    storage: &Path,
    temporary_parent: &Path,
    context: &RemoteBackupContext,
) -> Result<PreparedRemoteDatabase, CrewImportError> {
    context.validate()?;
    let host_sha256 = crate::crew_backup::hash(database)?;
    let edge_sha256 = crate::crew_backup::hash(storage)?;
    let mut raw = Vec::new();
    crate::crew_backup::file(storage)?.read_to_end(&mut raw)?;
    let edge_bytes = reconstruct_storage(&raw, context)?;
    let mut file = tempfile::NamedTempFile::new_in(temporary_parent)?;
    std::io::copy(&mut crate::crew_backup::file(database)?, file.as_file_mut())?;
    file.as_file().sync_all()?;
    let db = rusqlite::Connection::open(file.path())?;
    db.execute_batch("PRAGMA journal_mode=DELETE;")?;
    let host: Vec<u8> = db.query_row(
        "SELECT bytes FROM snapshots WHERE doc_id=?1",
        [&context.logical_session_id],
        |row| row.get(0),
    )?;
    let merged_bytes = crate::crew_desktop_backup::merge(&host, &edge_bytes)?;
    db.execute(
        "UPDATE snapshots SET bytes=?1 WHERE doc_id=?2",
        rusqlite::params![merged_bytes, context.logical_session_id],
    )?;
    drop(db);
    file.as_file().sync_all()?;
    if crate::crew_backup::hash(database)? != host_sha256
        || crate::crew_backup::hash(storage)? != edge_sha256
    {
        return Err(unsafe_data(
            "captured host or Edge source changed during reconciliation",
        ));
    }
    Ok(PreparedRemoteDatabase {
        file,
        host_sha256,
        edge_sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_remote_binding_never_accepts_unknown_generation_or_changed_epoch() {
        let context = RemoteBackupContext {
            migration_id: "migration".into(),
            owner_subject: "owner".into(),
            project_id: "project".into(),
            deployment_id: "deployment".into(),
            logical_session_id: "11111111-1111-4111-8111-111111111111".into(),
            sandbox_id: "sandbox".into(),
            device_id: "device".into(),
            lifecycle_epoch: 7,
            source_generation: "session".into(),
            writers_stopped: true,
        };
        let environment: SessionEnvironment = serde_json::from_value(serde_json::json!({
            "ownerPrincipal":"owner", "scope":context.scope(),
            "source":{"kind":"scaffold","sandbox_id":"sandbox","lifecycle":"ready","lifecycle_epoch":7}
        })).unwrap();
        assert!(context.environment(&environment).is_ok());
        let mut changed = context.clone();
        changed.lifecycle_epoch = 8;
        assert!(changed.environment(&environment).is_err());
        changed = context.clone();
        changed.source_generation = "unknown".into();
        assert!(changed.validate().is_err());
        changed = context.clone();
        changed.writers_stopped = false;
        assert!(changed.validate().is_err());
        changed = context.clone();
        changed.owner_subject = "other".into();
        assert!(changed.environment(&environment).is_err());
        assert!(
            context
                .document(
                    &context.logical_session_id,
                    "other-device",
                    &Value::Null,
                    &Value::Null
                )
                .is_err()
        );
    }
    #[test]
    fn frozen_storage_replays_pending_union_and_rejects_wrong_scope_or_missing_rows() {
        use base64::Engine;
        use serde_json::json;
        let context = RemoteBackupContext {
            migration_id: "migration".into(),
            owner_subject: "owner".into(),
            project_id: "project".into(),
            deployment_id: "deployment".into(),
            logical_session_id: "11111111-1111-4111-8111-111111111111".into(),
            sandbox_id: "sandbox".into(),
            device_id: "device".into(),
            lifecycle_epoch: 7,
            source_generation: "session".into(),
            writers_stopped: true,
        };
        let doc = LoroDoc::new();
        doc.get_map("meta")
            .insert("chatId", context.logical_session_id.as_str())
            .unwrap();
        doc.get_map("meta").insert("schemaVersion", 2i64).unwrap();
        doc.commit();
        let snapshot = doc.export(ExportMode::Snapshot).unwrap();
        let baseline = doc.oplog_vv();
        doc.get_map("meta").insert("first", "retained").unwrap();
        doc.commit();
        let first = doc.export(ExportMode::updates(&baseline)).unwrap();
        let first_version = doc.oplog_vv();
        doc.get_map("meta").insert("second", "retained").unwrap();
        doc.commit();
        let second = doc.export(ExportMode::updates(&first_version)).unwrap();
        let base64 = base64::engine::general_purpose::STANDARD;
        let source = json!({"schemaVersion":1,"format":"legacy-session-sql-v1","sourceGeneration":"session",
            "migrationId":"migration","scope":{"ownerSubject":"owner","projectId":"project","deploymentId":"deployment","logicalSessionId":context.logical_session_id},
            "frozenAt":"2026-10-21T00:00:00Z","meta":[],
            "blobs":[{"name":"snapshot","idx":0,"bytesBase64":base64.encode(&snapshot)}],
            "updates":[{"seq":3,"receivedAt":1,"bytesBase64":base64.encode(&second)},
                       {"seq":4,"receivedAt":2,"bytesBase64":base64.encode(&first)}]});
        let bytes = reconstruct_storage(&serde_json::to_vec(&source).unwrap(), &context).unwrap();
        let restored = LoroDoc::new();
        import_complete(&restored, &bytes).unwrap();
        assert_eq!(restored.oplog_vv(), doc.oplog_vv());
        for scenario in ["scope", "migration", "chunk", "order", "missing"] {
            let mut wrong = source.clone();
            match scenario {
                "scope" => wrong["scope"]["ownerSubject"] = json!("another-owner"),
                "migration" => wrong["migrationId"] = json!("another-migration"),
                "chunk" => wrong["blobs"][0]["idx"] = json!(1),
                "order" => wrong["updates"][1]["seq"] = json!(3),
                "missing" => {
                    wrong["updates"].as_array_mut().unwrap().pop();
                }
                _ => unreachable!(),
            }
            assert!(
                reconstruct_storage(&serde_json::to_vec(&wrong).unwrap(), &context).is_err(),
                "accepted {scenario}"
            );
        }
        let mut unscoped = source.clone();
        unscoped["scope"]["deploymentId"] = Value::Null;
        let bytes = serde_json::to_vec(&unscoped).unwrap();
        assert!(
            reconstruct_frozen_storage(&bytes, &unscoped["scope"], "session", "migration").is_ok()
        );
        assert!(
            reconstruct_storage(&bytes, &context).is_err(),
            "cloud scope cannot accept a desktop unscoped room"
        );
    }
}
