//! Offline conversion of authenticated coordinator terminal-source-authority
//! responses. Files bind evidence; the coordinator must independently revalidate
//! its own CP/provider facts and native readback before accepting publication.
use crate::crew_desktop_backup::PreparedDesktopDatabase;
use crate::crew_import::{CrewImportError, OriginalEpochProof};
use crate::crew_import_epoch::{OriginalEpochScope, OriginalEpochTermination};
use crate::crew_remote_backup::RemoteBackupContext;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use zeron_proto::{CollaborationPrincipal, CrewChatMetadata, OriginalEpochHistory};

fn unsafe_data(message: impl Into<String>) -> CrewImportError {
    CrewImportError::Unsafe(message.into())
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Authority {
    version: u32,
    kind: String,
    scope: OriginalEpochScope,
    coordinator_lifecycle_epoch: u64,
    migration_id: String,
    source_sha256: String,
    terminal: OriginalEpochTermination,
}
struct Source {
    authority: Authority,
    path: PathBuf,
    sha256: String,
}
#[derive(Default)]
pub(crate) struct TerminalBackups {
    sources: BTreeMap<String, Source>,
    pub metadata: BTreeMap<String, CrewChatMetadata>,
    pub markers: BTreeMap<String, OriginalEpochHistory>,
    pub evidence: BTreeMap<String, PathBuf>,
    pub hashes: BTreeMap<String, String>,
    files: Vec<tempfile::NamedTempFile>,
}
impl TerminalBackups {
    pub(crate) fn load(
        root: Option<&Path>,
        desktop: Option<&PreparedDesktopDatabase>,
    ) -> Result<Self, CrewImportError> {
        let mut result = Self::default();
        let Some(root) = root else {
            return Ok(result);
        };
        let desktop = desktop.ok_or_else(|| {
            unsafe_data("terminal authority requires the exact desktop source witness")
        })?;
        for entry in std::fs::read_dir(root.join("sessions"))? {
            let entry = entry?;
            let path = entry.path();
            let id = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| unsafe_data("invalid terminal authority filename"))?;
            if path.extension().and_then(|value| value.to_str()) != Some("json")
                || !zeron_proto::is_cloud_session_id(id)
            {
                return Err(unsafe_data(
                    "terminal authority directory contains an unknown source",
                ));
            }
            let sha256 = crate::crew_backup::hash(&path)?;
            let authority: Authority = serde_json::from_reader(crate::crew_backup::file(&path)?)?;
            let raw = format!("source-evidence/desktop/sessions/{id}.json");
            let original_scope =
                desktop.proof.document_scopes.get(id).ok_or_else(|| {
                    unsafe_data("terminal authority lacks an original replica scope")
                })?;
            if authority.version != 1
                || authority.kind != "terminal-source-authority"
                || authority.scope.logical_session_id != id
                || authority.scope.lifecycle_epoch == 0
                || authority.coordinator_lifecycle_epoch < authority.scope.lifecycle_epoch
                || authority.migration_id != desktop.proof.source_migration_id(id)?
                || authority.scope.owner_subject != desktop.proof.owner_subject
                || authority.scope.project_id != desktop.proof.project_id
                || authority.scope.deployment_id != desktop.proof.deployment_id
                || original_scope.get("deployment").and_then(Value::as_str)
                    != Some(authority.scope.deployment_id.as_str())
                || desktop.proof.source_files.get(&raw) != Some(&authority.source_sha256)
                || desktop.evidence.get(&raw).is_none()
            {
                return Err(unsafe_data(
                    "terminal authority differs from exact frozen source/replica scope or migration",
                ));
            }
            if crate::crew_backup::hash(desktop.evidence.get(&raw).unwrap())?
                != authority.source_sha256
                || crate::crew_backup::hash(&path)? != sha256
            {
                return Err(unsafe_data(
                    "terminal coordinator response or frozen source changed",
                ));
            }
            if result
                .sources
                .insert(
                    id.into(),
                    Source {
                        authority,
                        path,
                        sha256,
                    },
                )
                .is_some()
            {
                return Err(unsafe_data("duplicate terminal authority"));
            }
        }
        Ok(result)
    }

    pub(crate) fn validate_ref_environment(
        &self,
        id: &str,
        value: &Value,
    ) -> Result<(), CrewImportError> {
        let metadata = self
            .metadata
            .get(id)
            .ok_or_else(|| unsafe_data("unproven original epoch membership"))?;
        let marker = &self.markers[id];
        let environment: zeron_proto::SessionEnvironment = serde_json::from_value(value.clone())?;
        if environment.owner_principal != metadata.owner_subject
            || environment.scope != metadata.scope
            || !matches!(&environment.source, zeron_proto::SessionEnvironmentSource::Scaffold { sandbox_id, lifecycle_epoch, .. }
                if sandbox_id == &marker.sandbox_id && *lifecycle_epoch == Some(marker.lifecycle_epoch))
        {
            return Err(unsafe_data(
                "original epoch membership environment differs from exact source evidence",
            ));
        }
        Ok(())
    }

    pub(crate) fn prepare(
        &mut self,
        rows: &serde_json::Map<String, Value>,
        documents: &BTreeMap<String, Value>,
        principal: &CollaborationPrincipal,
        temporary_parent: &Path,
    ) -> Result<(), CrewImportError> {
        for (id, source) in &self.sources {
            let row = rows.get(id).ok_or_else(|| {
                unsafe_data(format!(
                    "original epoch has no actual replica host/config row: {id}"
                ))
            })?;
            let doc = documents.get(id).ok_or_else(|| {
                unsafe_data(format!(
                    "original epoch has no complete source snapshot: {id}"
                ))
            })?;
            if row.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                return Err(unsafe_data("original epoch host row identity mismatch"));
            }
            let config: zeron_proto::ChatConfig = serde_json::from_value(
                row.get("config")
                    .cloned()
                    .ok_or_else(|| unsafe_data("original epoch source config is missing"))?,
            )?;
            let native = row
                .get("harnessSessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty());
            if row
                .get("harnessSessionId")
                .is_some_and(|value| !value.is_null() && !value.is_string())
                || row
                    .get("harnessSessionCwd")
                    .or_else(|| row.get("cwd"))
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                return Err(unsafe_data(
                    "original epoch source native/cwd association is missing or malformed",
                ));
            }
            let mut outcomes = BTreeMap::<String, String>::new();
            if let Some(commands) = doc.get("commands") {
                for command in commands
                    .as_array()
                    .ok_or_else(|| unsafe_data("invalid original epoch commands"))?
                {
                    let id = command
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| unsafe_data("original command identity missing"))?;
                    let status = command
                        .get("status")
                        .and_then(Value::as_str)
                        .ok_or_else(|| unsafe_data("original command outcome missing"))?;
                    if outcomes.insert(id.into(), status.into()).is_some() {
                        return Err(unsafe_data("duplicate original command identity"));
                    }
                }
            }
            let authority = &source.authority;
            let proof = OriginalEpochProof {
                version: 1,
                kind: "terminal-original-epoch".into(),
                scope: authority.scope.clone(),
                migration_id: authority.migration_id.clone(),
                coordinator_lifecycle_epoch: authority.coordinator_lifecycle_epoch,
                source_sha256: authority.source_sha256.clone(),
                native_session_id: native.map(str::to_owned),
                terminal: authority.terminal.clone(),
                command_outcomes_sha256: format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&outcomes)?)
                ),
            };
            let bytes = serde_json::to_vec(&proof)?;
            let marker = crate::crew_import::validate_original_epoch_bootstrap(
                &bytes, &outcomes, principal,
            )?;
            let context = RemoteBackupContext {
                migration_id: authority.migration_id.clone(),
                owner_subject: authority.scope.owner_subject.clone(),
                project_id: authority.scope.project_id.clone(),
                deployment_id: authority.scope.deployment_id.clone(),
                logical_session_id: id.clone(),
                sandbox_id: authority.scope.sandbox_id.clone(),
                device_id: authority.scope.device_id.clone(),
                lifecycle_epoch: authority.scope.lifecycle_epoch,
                source_generation: "session".into(),
                writers_stopped: true,
            };
            let device = row
                .get("deviceId")
                .and_then(Value::as_str)
                .ok_or_else(|| unsafe_data("original epoch host device missing"))?;
            let mut metadata =
                context.original_epoch_document(id, device, row, doc, config.harness)?;
            metadata.original_epoch_history = Some(marker.clone());
            let mut file = tempfile::NamedTempFile::new_in(temporary_parent)?;
            file.write_all(&bytes)?;
            file.as_file().sync_all()?;
            let proof_path = format!("source-evidence/original-epochs/{id}.json");
            let authority_path = format!("source-evidence/terminal-authority/{id}.json");
            self.evidence
                .insert(proof_path.clone(), file.path().to_owned());
            self.hashes
                .insert(proof_path, marker.source_evidence_sha256.clone());
            self.evidence
                .insert(authority_path.clone(), source.path.clone());
            self.hashes.insert(authority_path, source.sha256.clone());
            self.files.push(file);
            self.metadata.insert(id.clone(), metadata);
            self.markers.insert(id.clone(), marker);
        }
        Ok(())
    }
}
