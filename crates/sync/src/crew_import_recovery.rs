//! A known interrupted legacy request, not a guessed prompt or an execution replay.
use crate::crew_import::{CrewBackupManifest, CrewImportError, CrewImportReport};
use loro::{LoroDoc, ToJson};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};
use zeron_proto::{CollaborationPrincipal, HarnessId, RunRequest};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportedRecoveryIntent {
    pub command_id: String,
    pub chat_id: String,
    pub source_chat_id: String,
    pub source_device_id: String,
    pub destination_device_id: String,
    pub owner_subject: String,
    pub project_id: String,
    pub deployment_id: Option<String>,
    pub native_session_id: String,
    pub harness: HarnessId,
    pub request: RunRequest,
    pub agent_account_id: Option<String>,
    pub user_message_id: String,
    pub updated_at: i64,
    pub journal_seq: u64,
    pub source_file: String,
    pub source_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingRun {
    identity: Identity,
    device_id: String,
    harness: HarnessId,
    request: Value,
    user_message_id: String,
    blocked_session: Option<String>,
    updated_at: i64,
    #[serde(default)]
    journal_seq: u64,
}
#[derive(Deserialize, PartialEq)]
enum Identity {
    SignedIn {
        owner_subject: String,
        project_scope: String,
    },
    SignedOut,
    Unattached,
}
fn unsafe_data(message: impl Into<String>) -> CrewImportError {
    CrewImportError::Unsafe(message.into())
}

fn read_intent(
    root: &Path,
    source: &str,
    manifest: &CrewBackupManifest,
    principal: &CollaborationPrincipal,
    device: &str,
) -> Result<ImportedRecoveryIntent, CrewImportError> {
    if !source.starts_with("legacy-journals/")
        || Path::new(source).extension().and_then(|s| s.to_str()) != Some("recovery")
        || !Path::new(source)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(unsafe_data("invalid recovery evidence path"));
    }
    let root = root.canonicalize()?;
    let path = root.join(source).canonicalize()?;
    if !path.starts_with(&root) {
        return Err(unsafe_data("recovery evidence escapes profile"));
    }
    let bytes = std::fs::read(&path)?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    if manifest.files.get(source) != Some(&digest) {
        return Err(unsafe_data("recovery evidence hash mismatch"));
    }
    let (source_chat_id, pending): (String, PendingRun) = serde_json::from_slice(&bytes)?;
    let stem: String = source_chat_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if path.file_stem().and_then(|s| s.to_str()) != Some(stem.as_str()) {
        return Err(unsafe_data("recovery evidence identity mismatch"));
    }
    let chat_id = crate::crew_import_aliases::canonical_execution_id(&source_chat_id)
        .unwrap_or(&source_chat_id)
        .to_owned();
    let binding = manifest
        .chats
        .get(&chat_id)
        .ok_or_else(|| unsafe_data("recovery has no verified chat binding"))?;
    binding.validate().map_err(unsafe_data)?;
    let context_relative = Path::new(source)
        .with_extension("context")
        .to_str()
        .ok_or_else(|| unsafe_data("non-UTF8 recovery context"))?
        .to_owned();
    let context_path = root.join(&context_relative).canonicalize()?;
    if !context_path.starts_with(&root) {
        return Err(unsafe_data("recovery context escapes profile"));
    }
    let context_bytes = std::fs::read(context_path)?;
    if manifest.files.get(&context_relative)
        != Some(&format!("{:x}", Sha256::digest(&context_bytes)))
    {
        return Err(unsafe_data("recovery context evidence hash mismatch"));
    }
    let (context_chat, (context_identity, context_device, context_harness, mut context_request)): (
        String,
        (Identity, String, HarnessId, Value),
    ) = serde_json::from_slice(&context_bytes)?;
    if context_chat != source_chat_id
        || context_identity != pending.identity
        || context_device != pending.device_id
        || context_harness != pending.harness
    {
        return Err(unsafe_data("recovery context authority differs"));
    }
    if !context_request.is_object() || !pending.request.is_object() {
        return Err(unsafe_data("recovery context request must be an object"));
    }
    // SessionStarted may advance the sidecar's native ID after context capture.
    // Nothing else in the accepted request is reconstructed or changed.
    let old_resume = context_request
        .get("resume")
        .filter(|value| !value.is_null());
    if old_resume.is_some_and(|value| Some(value) != pending.request.get("resume")) {
        return Err(unsafe_data("recovery context native identity conflicts"));
    }
    if let Some(resume) = pending.request.get("resume") {
        context_request["resume"] = resume.clone();
    }
    if context_request != pending.request {
        return Err(unsafe_data(
            "recovery full request differs from accepted context",
        ));
    }
    let Identity::SignedIn {
        owner_subject,
        project_scope,
    } = pending.identity
    else {
        return Err(unsafe_data(
            "recovery has no verified signed-in source identity",
        ));
    };
    if !manifest.writer_stopped
        || principal.session_id.is_some()
        || principal.subject != manifest.owner_subject
        || principal.project_id != manifest.project_id
        || principal.deployment_id != manifest.deployment_id
        || owner_subject != principal.subject
        || project_scope != principal.project_id
        || binding.owner_subject != owner_subject
        || binding.scope.project_id != project_scope
        || (binding.scope.deployment_id != principal.deployment_id
            && !(zeron_proto::is_local_history_chat(&chat_id)
                && binding.scope.deployment_id.is_none()
                && manifest.desktop_source.as_ref().is_some_and(|proof| {
                    proof
                        .document_scopes
                        .get(&chat_id)
                        .is_some_and(|scope| scope.get("deployment").is_some_and(Value::is_null))
                })))
        || pending.device_id.is_empty()
        || device.is_empty()
        || pending.blocked_session.is_some()
        || pending.harness != HarnessId::Omp
        || pending.user_message_id.is_empty()
    {
        return Err(unsafe_data(
            "recovery owner, device, native harness or execution outcome is unresolved",
        ));
    }
    let mut raw_request = pending.request;
    let fields = raw_request
        .as_object_mut()
        .ok_or_else(|| unsafe_data("recovery request is not an object"))?;
    if fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "prompt"
                | "model"
                | "agentAccountId"
                | "reasoning"
                | "modelOptions"
                | "cwd"
                | "sandbox"
                | "autoApprove"
                | "resume"
                | "attachments"
        )
    }) {
        return Err(unsafe_data(
            "unknown recovery request fields require explicit conversion",
        ));
    }
    let account = fields
        .remove("agentAccountId")
        .filter(|value| !value.is_null());
    let agent_account_id = account
        .map(|value| {
            value
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| unsafe_data("invalid recovery account selection"))
        })
        .transpose()?;
    // Legacy's omitted autoApprove default was true; preserve it exactly.
    fields.entry("autoApprove").or_insert(Value::Bool(true));
    let mut request: RunRequest = serde_json::from_value(raw_request)?;
    request.harness = Some(HarnessId::Omp);
    if let Some(account) = &agent_account_id {
        match request.model_options.get("crewAccountId") {
            Some(value) if value.as_str() != Some(account.as_str()) => {
                return Err(unsafe_data("conflicting recovery account selection"));
            }
            None => {
                request
                    .model_options
                    .insert("crewAccountId".into(), account.clone().into());
            }
            _ => {}
        }
    }
    if agent_account_id.is_none() && request.model_options.contains_key("crewAccountId") {
        return Err(unsafe_data(
            "recovery model options would change the original automatic account route",
        ));
    }
    let native = request
        .resume
        .as_ref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| unsafe_data("recovery has no exact native resume identity"))?
        .clone();
    if request.cwd.is_empty() || !manifest.native_histories.contains_key(&native) {
        return Err(unsafe_data(
            "recovery native history or original cwd is missing",
        ));
    }
    let events_relative = Path::new(source)
        .with_extension("jsonl")
        .to_str()
        .ok_or_else(|| unsafe_data("non-UTF8 recovery journal"))?
        .to_owned();
    let events_path = root.join(&events_relative).canonicalize()?;
    if !events_path.starts_with(&root) {
        return Err(unsafe_data("recovery event evidence escapes profile"));
    }
    let event_bytes = std::fs::read(events_path)?;
    if manifest.files.get(&events_relative) != Some(&format!("{:x}", Sha256::digest(&event_bytes)))
    {
        return Err(unsafe_data("recovery event evidence hash mismatch"));
    }
    let mut last = None;
    let mut previous = None;
    for line in event_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: Value = serde_json::from_slice(line)?;
        let seq = event
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or_else(|| unsafe_data("recovery event has no sequence"))?;
        if previous.is_some_and(|prior| seq <= prior) {
            return Err(unsafe_data("recovery events are not ordered"));
        }
        previous = Some(seq);
        last = Some(event);
    }
    let last = last.ok_or_else(|| unsafe_data("recovery has no durable interrupted outcome"))?;
    if last.pointer("/event/type").and_then(Value::as_str) != Some("done")
        || last.pointer("/event/status").and_then(Value::as_str) != Some("interrupted")
        || last["seq"]
            .as_u64()
            .is_none_or(|seq| seq <= pending.journal_seq)
    {
        return Err(unsafe_data(
            "recovery is terminal or unknown, not a proven interrupted request",
        ));
    }
    if last
        .pointer("/event/sessionId")
        .and_then(Value::as_str)
        .is_some_and(|id| id != native)
    {
        return Err(unsafe_data("recovery terminal native identity differs"));
    }
    let command_id = format!(
        "legacy-recovery-{:x}",
        Sha256::digest(serde_json::to_vec(&(
            &owner_subject,
            &project_scope,
            &principal.deployment_id,
            &source_chat_id,
            &digest,
            &pending.user_message_id,
        ))?)
    );
    Ok(ImportedRecoveryIntent {
        command_id,
        chat_id,
        source_chat_id,
        source_device_id: pending.device_id,
        destination_device_id: device.into(),
        owner_subject,
        project_id: project_scope,
        deployment_id: principal.deployment_id.clone(),
        native_session_id: native,
        harness: HarnessId::Omp,
        request,
        agent_account_id,
        user_message_id: pending.user_message_id,
        updated_at: pending.updated_at,
        journal_seq: pending.journal_seq,
        source_file: source.into(),
        source_sha256: digest,
    })
}

pub(crate) fn prepare(
    root: &Path,
    manifest: &CrewBackupManifest,
    principal: &CollaborationPrincipal,
    device: &str,
    states: &mut BTreeMap<String, String>,
    documents: &BTreeMap<String, LoroDoc>,
) -> Result<Vec<ImportedRecoveryIntent>, CrewImportError> {
    let workspace = documents
        .get("workspace3")
        .ok_or_else(|| unsafe_data("recovery workspace is missing"))?
        .get_deep_value()
        .to_json_value();
    let mut intents = Vec::new();
    for (source, state) in states
        .iter_mut()
        .filter(|(_, state)| state.as_str() == "pending-recovery")
    {
        let intent = read_intent(root, source, manifest, principal, device)?;
        let row = workspace
            .get("chats")
            .and_then(|rows| rows.get(&intent.chat_id))
            .ok_or_else(|| unsafe_data("recovery has no canonical host row"))?;
        if row.get("deviceId").and_then(Value::as_str) != Some(intent.source_device_id.as_str())
            || row.get("harnessSessionId").and_then(Value::as_str)
                != Some(intent.native_session_id.as_str())
            || row
                .get("harnessSessionCwd")
                .or_else(|| row.get("cwd"))
                .and_then(Value::as_str)
                != Some(intent.request.cwd.as_str())
            || row.pointer("/config/harness").and_then(Value::as_str) != Some("omp")
        {
            return Err(unsafe_data(
                "recovery source host/native/worktree association differs",
            ));
        }
        let doc = documents
            .get(&intent.chat_id)
            .ok_or_else(|| unsafe_data("recovery transcript missing"))?
            .get_deep_value()
            .to_json_value();
        if doc
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter()
                    .rev()
                    .find(|row| row.get("role").and_then(Value::as_str) == Some("user"))
            })
            .and_then(|row| row.get("id"))
            .and_then(Value::as_str)
            != Some(intent.user_message_id.as_str())
        {
            return Err(unsafe_data("recovery is not the exact latest user request"));
        }
        if row.pointer("/config/model").and_then(Value::as_str) != intent.request.model.as_deref() {
            return Err(unsafe_data(
                "recovery model differs from source chat configuration",
            ));
        }
        if intents
            .iter()
            .any(|prior: &ImportedRecoveryIntent| prior.chat_id == intent.chat_id)
        {
            return Err(unsafe_data(
                "multiple pending requests target one recovered chat",
            ));
        }
        *state = "validated-interrupted-request".into();
        intents.push(intent);
    }
    Ok(intents)
}

/// Recheck immutable source bindings and bytes on every runtime load. The engine
/// separately checks current verified authority/device and admits command_id once.
pub fn imported_recovery_intents(
    profile: &Path,
) -> Result<Vec<ImportedRecoveryIntent>, CrewImportError> {
    if profile.join("crew-import-incomplete").exists() {
        return Err(unsafe_data("recovery import is incomplete"));
    }
    let report_bytes = match std::fs::read(profile.join("crew-import-report.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let report: CrewImportReport = serde_json::from_slice(&report_bytes)?;
    if report.version != 1 {
        return Err(unsafe_data("unsupported recovery import report"));
    }
    if report.recovery_intent_ids.is_empty() {
        return Ok(Vec::new());
    }
    let source = std::fs::read(profile.join("crew-import-source.json"))?;
    if format!("{:x}", Sha256::digest(&source)) != report.source_manifest_sha256 {
        return Err(unsafe_data(
            "imported recovery source manifest integrity differs",
        ));
    }
    let manifest: CrewBackupManifest = serde_json::from_slice(&source)?;
    let principal = CollaborationPrincipal {
        subject: report.owner_subject.clone(),
        email: None,
        project_id: report.project_id.clone(),
        deployment_id: report.deployment_id.clone(),
        session_id: None,
        capabilities: Vec::new(),
        unknown: Default::default(),
    };
    if let Some(proof) = &manifest.desktop_source {
        proof.validate_evidence(profile, &manifest.files, &principal)?;
    }
    // Also binds each native archive's header, path jail and untouched bytes.
    super::crew_import::imported_native_histories(profile)?;
    let db = Connection::open_with_flags(
        profile.join("docs.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let records = db.prepare("SELECT value FROM legacy_crew_records WHERE container='recovery-intent' ORDER BY doc_id,record_id")?
        .query_map([], |row| row.get::<_, Vec<u8>>(0))?.collect::<Result<Vec<_>, _>>()?;
    let mut result = Vec::new();
    for bytes in records {
        let saved: ImportedRecoveryIntent = serde_json::from_slice(&bytes)?;
        let actual = read_intent(
            profile,
            &saved.source_file,
            &manifest,
            &principal,
            &report.device_id,
        )?;
        if actual != saved
            || report.recovery_intent_ids.get(&saved.chat_id) != Some(&saved.command_id)
        {
            return Err(unsafe_data(
                "imported recovery identity/request evidence differs",
            ));
        }
        result.push(saved);
    }
    if result.len() != report.recovery_intent_ids.len() {
        return Err(unsafe_data("imported recovery membership differs"));
    }
    Ok(result)
}
