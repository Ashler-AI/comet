//! Original-epoch history evidence. This never declares a logical session retired.
use crate::crew_import::{CrewBackupManifest, CrewImportError, CrewImportReport};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet}, path::Path};
use zeron_proto::{CollaborationPrincipal, OriginalEpochHistory};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OriginalEpochProof {
    pub version: u32,
    pub kind: String,
    pub scope: OriginalEpochScope,
    pub coordinator_lifecycle_epoch: u64,
    pub migration_id: String,
    pub source_sha256: String,
    pub native_session_id: Option<String>,
    pub terminal: OriginalEpochTermination,
    pub command_outcomes_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OriginalEpochScope {
    pub owner_subject: String,
    pub project_id: String,
    pub deployment_id: String,
    pub logical_session_id: String,
    pub sandbox_id: String,
    pub device_id: String,
    pub lifecycle_epoch: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OriginalEpochTermination {
    pub status: String,
    pub provider_action: String,
    pub terminated_at: String,
    pub termination_reason: String,
    pub physical_sandbox_id: String,
    pub provider_not_found_observed_at: String,
    pub retained_snapshot_ids: Vec<String>,
}
fn unsafe_data(message: impl Into<String>) -> CrewImportError {
    CrewImportError::Unsafe(message.into())
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn evidence(
    root: &Path,
    files: &BTreeMap<String, String>,
    digest: &str,
) -> Result<Vec<u8>, CrewImportError> {
    if !valid_digest(digest) {
        return Err(unsafe_data("invalid original-epoch evidence digest"));
    }
    let relative = files
        .iter()
        .find(|(path, hash)| path.starts_with("source-evidence/") && hash.as_str() == digest)
        .map(|(path, _)| path)
        .ok_or_else(|| unsafe_data("original-epoch proof/source is not inventoried"))?;
    if !Path::new(relative)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(unsafe_data("invalid original-epoch evidence path"));
    }
    let root = root.canonicalize()?;
    let path = root.join(relative).canonicalize()?;
    if !path.starts_with(&root) {
        return Err(unsafe_data("original-epoch evidence escapes profile"));
    }
    let bytes = std::fs::read(path)?;
    if sha(&bytes) != digest {
        return Err(unsafe_data("original-epoch evidence integrity mismatch"));
    }
    Ok(bytes)
}

fn validate_proof(
    proof: &OriginalEpochProof,
    proof_bytes: &[u8],
    principal: &CollaborationPrincipal,
) -> Result<OriginalEpochHistory, CrewImportError> {
    let scope = &proof.scope;
    let terminal = &proof.terminal;
    let terminated = chrono::DateTime::parse_from_rfc3339(&terminal.terminated_at)
        .map_err(|_| unsafe_data("invalid original-epoch termination timestamp"))?;
    let observed = chrono::DateTime::parse_from_rfc3339(&terminal.provider_not_found_observed_at)
        .map_err(|_| unsafe_data("invalid original-epoch absence timestamp"))?;
    if proof.version != 1
        || proof.kind != "terminal-original-epoch"
        || proof.migration_id.trim().is_empty()
        || scope.owner_subject != principal.subject
        || scope.project_id != principal.project_id
        || Some(scope.deployment_id.as_str()) != principal.deployment_id.as_deref()
        || principal
            .session_id
            .as_ref()
            .is_some_and(|id| id != &scope.logical_session_id)
        || !zeron_proto::is_cloud_session_id(&scope.logical_session_id)
        || scope.lifecycle_epoch == 0
        || proof.coordinator_lifecycle_epoch < scope.lifecycle_epoch
        || scope.sandbox_id.is_empty()
        || scope.device_id.is_empty()
        || (scope.device_id.strip_prefix("comet-scaffold-") != Some(scope.sandbox_id.as_str())
            && zeron_proto::parse_scaffold_device_id(&scope.device_id)
                != Some((scope.sandbox_id.as_str(), scope.lifecycle_epoch)))
        || !valid_digest(&proof.source_sha256)
        || !valid_digest(&proof.command_outcomes_sha256)
        || proof
            .native_session_id
            .as_deref()
            .is_some_and(str::is_empty)
        || !matches!(terminal.status.as_str(), "stopped" | "failed")
        || terminal.provider_action != "kill"
        || !matches!(
            terminal.termination_reason.as_str(),
            "timeout" | "provider_missing"
        )
        || terminal.physical_sandbox_id != scope.sandbox_id
        || observed < terminated
        || !terminal.retained_snapshot_ids.is_empty()
    {
        return Err(unsafe_data(
            "original epoch lacks exact server termination, physical absence and snapshot-absence evidence",
        ));
    }
    Ok(OriginalEpochHistory {
        sandbox_id: scope.sandbox_id.clone(),
        lifecycle_epoch: scope.lifecycle_epoch,
        coordinator_lifecycle_epoch: proof.coordinator_lifecycle_epoch,
        source_device_id: scope.device_id.clone(),
        original_status: terminal.status.clone(),
        native_session_id: proof.native_session_id.clone(),
        source_evidence_sha256: sha(proof_bytes),
    })
}

/// Validate an authenticated control-plane bootstrap receipt and its full source outcomes.
/// The caller must verify the actual raw source SHA upstream; this function must not
/// be used to promote untrusted metadata or caller-supplied booleans into authority.
/// Preserve the original JSON bytes: their hash, not a reserialization, binds the marker.
pub fn validate_original_epoch_bootstrap(
    proof_bytes: &[u8],
    command_outcomes: &BTreeMap<String, String>,
    principal: &CollaborationPrincipal,
) -> Result<OriginalEpochHistory, CrewImportError> {
    let proof: OriginalEpochProof = serde_json::from_slice(proof_bytes)?;
    let marker = validate_proof(&proof, proof_bytes, principal)?;
    verify_outcomes(&proof, command_outcomes)?;
    Ok(marker)
}

#[cfg(test)]
fn persist_original_epoch_bootstrap(
    store: &crate::DocsStore,
    proof_json: &str,
    outcomes: &BTreeMap<String, String>,
    principal: &CollaborationPrincipal,
) -> Result<OriginalEpochHistory, CrewImportError> {
    let history = validate_original_epoch_bootstrap(proof_json.as_bytes(), outcomes, principal)?;
    let proof: OriginalEpochProof = serde_json::from_str(proof_json)?;
    let key = format!("original-epoch-bootstrap:{}:{}", proof.scope.logical_session_id, history.lifecycle_epoch);
    let request = serde_json::to_vec(&serde_json::json!({
        "proofJson":proof_json, "commandOutcomes":outcomes, "principal":principal,
    }))?;
    store.claim_crew_command(&key, &request)?;
    for id in outcomes.keys() { store.mark_processed(id)?; }
    store.finish_crew_command(&key, &serde_json::to_vec(&history)?)?;
    Ok(history)
}

/// Return only the selected session's original receipt from a freshly verified profile report.
pub fn imported_original_epoch_proof_json(
    profile: &Path,
    report: &CrewImportReport,
) -> Result<Option<String>, CrewImportError> {
    let selected = report
        .selected_chat_id
        .as_ref()
        .ok_or_else(|| unsafe_data("original-epoch proof requires a selected session report"))?;
    let Some(marker) = report.original_epoch_histories.get(selected) else {
        return Ok(None);
    };
    let bytes = evidence(
        profile,
        &report.files_sha256,
        &marker.source_evidence_sha256,
    )?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| unsafe_data("original-epoch proof is not UTF-8"))
}

/// The authenticated server/provider producer establishes these facts. Offline
/// files bind its receipt; neither stopped status nor a caller Boolean is proof.
pub(crate) fn validate(
    root: &Path,
    manifest: &CrewBackupManifest,
    principal: &CollaborationPrincipal,
) -> Result<BTreeMap<String, OriginalEpochProof>, CrewImportError> {
    let mut result = BTreeMap::new();
    for (id, marker) in &manifest.original_epoch_histories {
        let proof_bytes = evidence(root, &manifest.files, &marker.source_evidence_sha256)?;
        let proof: OriginalEpochProof = serde_json::from_slice(&proof_bytes)?;
        let expected = validate_proof(&proof, &proof_bytes, principal)?;
        let scope = &proof.scope;
        if !manifest.writer_stopped
            || principal.session_id.is_some()
            || manifest.owner_subject != principal.subject
            || manifest.project_id != principal.project_id
            || manifest.deployment_id != principal.deployment_id
            || scope.logical_session_id != *id
            || marker != &expected
        {
            return Err(unsafe_data(
                "original epoch differs from stopped-writer manifest authority or exact marker",
            ));
        }
        let metadata = manifest
            .chats
            .get(id)
            .ok_or_else(|| unsafe_data("original epoch has no chat binding"))?;
        metadata.validate().map_err(unsafe_data)?;
        if metadata.owner_subject != scope.owner_subject
            || metadata.scope.project_id != scope.project_id
            || metadata.scope.deployment_id.as_deref() != Some(scope.deployment_id.as_str())
            || metadata.session_id != *id
            || metadata.original_epoch_history.as_ref() != Some(marker)
        {
            return Err(unsafe_data(
                "original-epoch marker differs from exact imported chat binding",
            ));
        }
        let bytes = evidence(root, &manifest.files, &proof.source_sha256)?;
        crate::crew_remote_backup::reconstruct_frozen_storage(
            &bytes,
            &serde_json::json!({
                "ownerSubject":scope.owner_subject,"projectId":scope.project_id,"deploymentId":scope.deployment_id,
                "logicalSessionId":scope.logical_session_id,
            }),
            "session",
            &proof.migration_id,
        )?;
        result.insert(id.clone(), proof);
    }
    Ok(result)
}

pub(crate) fn verify_outcomes(
    proof: &OriginalEpochProof,
    outcomes: &BTreeMap<String, String>,
) -> Result<(), CrewImportError> {
    if sha(&serde_json::to_vec(outcomes)?) != proof.command_outcomes_sha256 {
        return Err(unsafe_data(
            "original-epoch immutable command outcomes differ from server evidence",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BootstrapRequest {
    proof_json: String,
    command_outcomes: BTreeMap<String, String>,
    principal: CollaborationPrincipal,
}

/// A single verified ledger snapshot. Command IDs fence historical rows even
/// when their terminal doc outcomes have not yet arrived over sync.
#[derive(Debug, Default)]
pub struct OriginalEpochHistories {
    pub histories: BTreeMap<String, OriginalEpochHistory>,
    pub command_ids: BTreeSet<String>,
}

/// Loaded into host-only runtime state, independent of remotely editable metadata.
pub fn imported_original_epoch_histories(
    profile: &Path,
) -> Result<OriginalEpochHistories, CrewImportError> {
    if profile.join("crew-import-incomplete").exists() {
        return Err(unsafe_data("original-epoch import is incomplete"));
    }
    let mut loaded = OriginalEpochHistories::default();
    let mut offline_proofs = BTreeMap::new();
    let bytes = match std::fs::read(profile.join("crew-import-report.json")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(bytes) = bytes {
        let report: CrewImportReport = serde_json::from_slice(&bytes)?;
        if report.version != 1 {
            return Err(unsafe_data("unsupported original-epoch import report"));
        }
        if !report.original_epoch_histories.is_empty() {
            let bytes = std::fs::read(profile.join("crew-import-source.json"))?;
            if sha(&bytes) != report.source_manifest_sha256 {
                return Err(unsafe_data("original-epoch source manifest hash differs"));
            }
            let manifest: CrewBackupManifest = serde_json::from_slice(&bytes)?;
            if manifest.original_epoch_histories != report.original_epoch_histories {
                return Err(unsafe_data(
                    "original-epoch runtime marker differs from source",
                ));
            }
            let principal = CollaborationPrincipal {
                subject: report.owner_subject,
                email: None,
                project_id: report.project_id,
                deployment_id: report.deployment_id,
                session_id: None,
                capabilities: Vec::new(),
                unknown: Default::default(),
            };
            offline_proofs = validate(profile, &manifest, &principal)?;
            loaded.histories = manifest.original_epoch_histories;
        }
    }
    let database = profile.join("docs.sqlite3");
    if !database.try_exists()? {
        if !offline_proofs.is_empty() {
            return Err(unsafe_data("original-epoch offline command ledger is missing"));
        }
        return Ok(loaded);
    }
    let mut conn = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let tx = conn.transaction()?;
    // Offline imports retain exact source commands separately from the mutable
    // session doc. Bind their complete outcome map to the already verified proof.
    for (chat_id, proof) in offline_proofs {
        let mut statement = tx.prepare(
            "SELECT record_id,value,EXISTS(SELECT 1 FROM processed_commands WHERE command_id=record_id) FROM legacy_crew_records WHERE doc_id=?1 AND container='commands'",
        )?;
        let rows = statement.query_map([chat_id], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, bool>(2)?,
        )))?;
        let mut outcomes = BTreeMap::new();
        for row in rows {
            let (id, bytes, processed) = row?;
            #[derive(Deserialize)]
            struct Command { id: String, status: String }
            let command: Command = serde_json::from_slice(&bytes)?;
            if command.id != id || !processed {
                return Err(unsafe_data("original-epoch offline command differs from its processed ledger"));
            }
            outcomes.insert(id, command.status);
        }
        verify_outcomes(&proof, &outcomes)?;
        loaded.command_ids.extend(outcomes.into_keys());
    }
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='crew_commands')",
        [],
        |row| row.get::<_, bool>(0),
    )? {
        return Ok(loaded);
    }
    // This internal ledger is installed only after fresh authenticated CP bootstrap,
    // under the profile lock. Its stored principal is not a caller authentication API.
    let mut statement=tx.prepare("SELECT command_id,request,outcome FROM crew_commands WHERE substr(command_id,1,length(?1))=?1")?;
    let rows = statement.query_map(["original-epoch-bootstrap:"], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
        ))
    })?;
    for row in rows {
        let (key, request, outcome) = row?;
        let outcome = outcome
            .ok_or_else(|| unsafe_data("original-epoch bootstrap installation is incomplete"))?;
        let request: BootstrapRequest = serde_json::from_slice(&request)?;
        let proof: OriginalEpochProof = serde_json::from_str(&request.proof_json)?;
        let marker = validate_original_epoch_bootstrap(
            request.proof_json.as_bytes(),
            &request.command_outcomes,
            &request.principal,
        )?;
        if key
            != format!(
                "original-epoch-bootstrap:{}:{}",
                proof.scope.logical_session_id, proof.scope.lifecycle_epoch
            )
            || serde_json::from_slice::<OriginalEpochHistory>(&outcome)? != marker
        {
            return Err(unsafe_data(
                "original-epoch bootstrap key or persisted outcome differs from proof",
            ));
        }
        for command in request.command_outcomes.keys() {
            if !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM processed_commands WHERE command_id=?1)",
                [command],
                |row| row.get::<_, bool>(0),
            )? {
                return Err(unsafe_data(
                    "original-epoch bootstrap command is not fenced as processed",
                ));
            }
        }
        if loaded.histories
            .get(&proof.scope.logical_session_id)
            .is_some_and(|existing| existing != &marker)
        {
            return Err(unsafe_data("conflicting original-epoch bootstrap history"));
        }
        loaded.histories.insert(proof.scope.logical_session_id, marker);
        loaded.command_ids.extend(request.command_outcomes.into_keys());
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bootstrap_fixture() -> (
        OriginalEpochProof,
        BTreeMap<String, String>,
        CollaborationPrincipal,
    ) {
        let id = "53a67c53-cbc2-4a86-bffd-4c9f5c735590";
        let outcomes = BTreeMap::from([
            ("command-a".into(), "completed".into()),
            ("command-b".into(), "interrupted".into()),
        ]);
        let proof = OriginalEpochProof {
            version: 1,
            kind: "terminal-original-epoch".into(),
            migration_id: "migration".into(),
            source_sha256: sha(b"source"),
            coordinator_lifecycle_epoch: 2,
            native_session_id: Some("native".into()),
            command_outcomes_sha256: sha(&serde_json::to_vec(&outcomes).unwrap()),
            scope: OriginalEpochScope {
                owner_subject: "owner".into(),
                project_id: "project".into(),
                deployment_id: "deployment".into(),
                logical_session_id: id.into(),
                sandbox_id: "old-sandbox".into(),
                device_id: "comet-scaffold-old-sandbox".into(),
                lifecycle_epoch: 1,
            },
            terminal: OriginalEpochTermination {
                status: "stopped".into(),
                provider_action: "kill".into(),
                termination_reason: "provider_missing".into(),
                terminated_at: "2026-10-21T01:00:00Z".into(),
                provider_not_found_observed_at: "2026-10-21T01:00:01Z".into(),
                physical_sandbox_id: "old-sandbox".into(),
                retained_snapshot_ids: vec![],
            },
        };
        let principal = CollaborationPrincipal {
            subject: "owner".into(),
            email: None,
            project_id: "project".into(),
            deployment_id: Some("deployment".into()),
            session_id: None,
            capabilities: vec![],
            unknown: Default::default(),
        };
        (proof, outcomes, principal)
    }

    #[test]
    fn bootstrap_binds_exact_raw_proof_outcomes_and_principal_scope() {
        let (proof, outcomes, mut principal) = bootstrap_fixture();
        let id = proof.scope.logical_session_id.clone();
        let bytes = serde_json::to_vec(&proof).unwrap();
        let marker = validate_original_epoch_bootstrap(&bytes, &outcomes, &principal).unwrap();
        assert_eq!(marker.source_evidence_sha256, sha(&bytes));
        assert_eq!(marker.native_session_id.as_deref(), Some("native"));
        assert_eq!(
            (marker.lifecycle_epoch, marker.coordinator_lifecycle_epoch),
            (1, 2)
        );
        let mut missing = serde_json::to_value(&proof).unwrap();
        missing
            .as_object_mut()
            .unwrap()
            .remove("coordinatorLifecycleEpoch");
        assert!(
            validate_original_epoch_bootstrap(
                &serde_json::to_vec(&missing).unwrap(),
                &outcomes,
                &principal
            )
            .is_err()
        );
        let mut older = proof.clone();
        older.coordinator_lifecycle_epoch = 0;
        assert!(
            validate_original_epoch_bootstrap(
                &serde_json::to_vec(&older).unwrap(),
                &outcomes,
                &principal
            )
            .is_err()
        );
        older.coordinator_lifecycle_epoch = 1;
        assert!(
            validate_original_epoch_bootstrap(
                &serde_json::to_vec(&older).unwrap(),
                &outcomes,
                &principal
            )
            .is_ok()
        );
        let mut original = proof.clone();
        original.scope.device_id = "comet-scaffold-old-sandbox-e1".into();
        assert!(
            validate_original_epoch_bootstrap(
                &serde_json::to_vec(&original).unwrap(),
                &outcomes,
                &principal
            )
            .is_ok()
        );
        for device in [
            "old-host",
            "comet-scaffold-other",
            "comet-scaffold-old-sandbox-e2",
        ] {
            original.scope.device_id = device.into();
            assert!(
                validate_original_epoch_bootstrap(
                    &serde_json::to_vec(&original).unwrap(),
                    &outcomes,
                    &principal
                )
                .is_err()
            );
        }
        principal.session_id = Some(id.into());
        assert_eq!(
            validate_original_epoch_bootstrap(&bytes, &outcomes, &principal).unwrap(),
            marker
        );
        let mut changed = outcomes.clone();
        changed.insert("command-a".into(), "failed".into());
        assert!(validate_original_epoch_bootstrap(&bytes, &changed, &principal).is_err());
        changed = outcomes.clone();
        changed.remove("command-b");
        assert!(validate_original_epoch_bootstrap(&bytes, &changed, &principal).is_err());
        for field in ["subject", "project_id", "deployment_id", "session_id"] {
            let mut foreign = principal.clone();
            match field {
                "subject" => foreign.subject = "other".into(),
                "project_id" => foreign.project_id = "other".into(),
                "deployment_id" => foreign.deployment_id = Some("other".into()),
                _ => foreign.session_id = Some("other".into()),
            }
            assert!(validate_original_epoch_bootstrap(&bytes, &outcomes, &foreign).is_err());
        }
        let raw = std::str::from_utf8(&bytes).unwrap();
        let reordered = format!(
            "{{{},\"version\":1}}",
            raw.strip_prefix("{\"version\":1,")
                .unwrap()
                .strip_suffix('}')
                .unwrap()
        );
        let reordered_marker =
            validate_original_epoch_bootstrap(reordered.as_bytes(), &outcomes, &principal).unwrap();
        assert_eq!(
            reordered_marker.source_evidence_sha256,
            sha(reordered.as_bytes())
        );
        assert_ne!(
            reordered_marker.source_evidence_sha256,
            marker.source_evidence_sha256
        );
        for invalid in [
            "source_sha256",
            "command_outcomes_sha256",
            "native_session_id",
        ] {
            let mut invalid_proof = proof.clone();
            match invalid {
                "source_sha256" => invalid_proof.source_sha256 = "A".repeat(64),
                "command_outcomes_sha256" => invalid_proof.command_outcomes_sha256 = "0".repeat(63),
                _ => invalid_proof.native_session_id = Some(String::new()),
            }
            assert!(
                validate_original_epoch_bootstrap(
                    &serde_json::to_vec(&invalid_proof).unwrap(),
                    &outcomes,
                    &principal
                )
                .is_err()
            );
        }
    }

    #[test]
    fn persisted_bootstrap_reloads_provenance_and_rejects_immutable_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::DocsStore::open(temp.path()).unwrap();
        let (proof, outcomes, principal) = bootstrap_fixture();
        let proof_json = serde_json::to_string(&proof).unwrap();
        let marker = persist_original_epoch_bootstrap(&store, &proof_json, &outcomes, &principal).unwrap();
        assert_eq!(persist_original_epoch_bootstrap(&store, &proof_json, &outcomes, &principal).unwrap(), marker);
        let key = format!("original-epoch-bootstrap:{}:{}", proof.scope.logical_session_id, proof.scope.lifecycle_epoch);
        let saved = store.crew_command(&key).unwrap();
        // Semantically identical proof with different raw bytes cannot replace the claim.
        let reformatted = serde_json::to_string_pretty(&proof).unwrap();
        assert!(persist_original_epoch_bootstrap(&store, &reformatted, &outcomes, &principal).is_err());
        let mut changed_outcomes = outcomes.clone();
        changed_outcomes.insert("new-command".into(), "completed".into());
        let mut changed_proof = proof.clone();
        changed_proof.command_outcomes_sha256 = sha(&serde_json::to_vec(&changed_outcomes).unwrap());
        assert!(persist_original_epoch_bootstrap(&store, &serde_json::to_string(&changed_proof).unwrap(), &changed_outcomes, &principal).is_err());
        assert!(!store.is_processed("new-command").unwrap());
        let mut changed_principal = principal.clone();
        changed_principal.email = Some("changed@example.com".into());
        assert!(persist_original_epoch_bootstrap(&store, &proof_json, &outcomes, &changed_principal).is_err());
        assert_eq!(store.crew_command(&key).unwrap(), saved);
        drop(store);
        let loaded = imported_original_epoch_histories(temp.path()).unwrap();
        assert_eq!(loaded.histories, BTreeMap::from([(proof.scope.logical_session_id, marker)]));
        assert_eq!(loaded.command_ids, outcomes.into_keys().collect());

        let conflicting = tempfile::tempdir().unwrap();
        let store = crate::DocsStore::open(conflicting.path()).unwrap();
        let (_, outcomes, principal) = bootstrap_fixture();
        let request = serde_json::to_vec(&serde_json::json!({
            "proofJson":proof_json, "commandOutcomes":outcomes, "principal":principal,
        })).unwrap();
        store.claim_crew_command(&key, &request).unwrap();
        let mut wrong = validate_original_epoch_bootstrap(proof_json.as_bytes(), &outcomes, &principal).unwrap();
        wrong.original_status = "failed".into();
        store.finish_crew_command(&key, &serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert!(persist_original_epoch_bootstrap(&store, &proof_json, &outcomes, &principal).is_err());
        assert!(imported_original_epoch_histories(conflicting.path()).is_err());
        assert_eq!(store.crew_command(&key).unwrap().unwrap().1, Some(serde_json::to_vec(&wrong).unwrap()));
    }

    #[test]
    fn bootstrap_only_profile_requires_completed_ledger_and_every_processed_command() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::DocsStore::open(temp.path()).unwrap();
        let (proof, outcomes, principal) = bootstrap_fixture();
        let proof_json = serde_json::to_string(&proof).unwrap();
        let marker =
            validate_original_epoch_bootstrap(proof_json.as_bytes(), &outcomes, &principal)
                .unwrap();
        let key = format!(
            "original-epoch-bootstrap:{}:{}",
            proof.scope.logical_session_id, proof.scope.lifecycle_epoch
        );
        let request=serde_json::to_vec(&serde_json::json!({"proofJson":proof_json,"commandOutcomes":outcomes,"principal":principal})).unwrap();
        store.claim_crew_command(&key, &request).unwrap();
        assert!(imported_original_epoch_histories(temp.path()).is_err());
        store
            .finish_crew_command(&key, &serde_json::to_vec(&marker).unwrap())
            .unwrap();
        store.mark_processed("command-a").unwrap();
        assert!(imported_original_epoch_histories(temp.path()).is_err());
        store.mark_processed("command-b").unwrap();
        assert_eq!(
            imported_original_epoch_histories(temp.path()).unwrap().histories,
            BTreeMap::from([(proof.scope.logical_session_id, marker)])
        );
    }
}
