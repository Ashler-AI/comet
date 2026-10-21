//! Native CLI operations use the upstream durable command plane and transcript watch.
use super::EngineRpc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use zeron_doc::SessionCommandPayload;
use zeron_harness::CancellationToken;
use zeron_proto::{AgentRoute, Chat, HarnessId, RunRequest, ScaffoldDatabaseEnvironment};
use zeron_rpc::{RpcError, RpcReply};

// ponytail: serialize native branch preparation; per-source gates if contention matters.
static NATIVE_GATE: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));
// A waiting reply's processed-ledger claim and command append are one admission.
static PEER_ADMISSION: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

type WaitKey = (String, String, String);
struct WaitOwner { id: String, after: Option<String>, sender: tokio::sync::oneshot::Sender<Value> }
static PEER_WAITS: LazyLock<parking_lot::Mutex<std::collections::HashMap<WaitKey, WaitOwner>>> =
    LazyLock::new(|| parking_lot::Mutex::new(Default::default()));
struct PeerWaitGuard { key: WaitKey, id: String, receiver: tokio::sync::oneshot::Receiver<Value> }
impl PeerWaitGuard {
    fn register(key: WaitKey, after: Option<&str>) -> Result<Self, RpcError> {
        let mut waits = PEER_WAITS.lock();
        if waits.contains_key(&key) { return Err(failed("A reply waiter already owns this session/thread")); }
        let id = crate::new_id();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        waits.insert(key.clone(), WaitOwner { id:id.clone(), after:after.map(str::to_owned), sender });
        Ok(Self { key, id, receiver })
    }
}
impl Drop for PeerWaitGuard {
    fn drop(&mut self) {
        let mut waits = PEER_WAITS.lock();
        if waits.get(&self.key).is_some_and(|owner| owner.id == self.id) { waits.remove(&self.key); }
    }
}

pub(super) fn handles(method: &str) -> bool {
    matches!(method, "ForkSession" | "HandoffSessionToScaffold" | "RecoverSessionHandoffToScaffold"
        | "HandoffSessionOnlyToScaffold" | "RecoverSessionOnlyHandoffToScaffold"
        | "SendPeerMessage" | "ReplyPeerMessage" | "WaitPeerReply" | "DeliverPeerMessage" | "ImportCrewBackup" | "ExportCrewBackup"
        | "VerifyImportedCrewSession" | "PublishImportedCrewSession")
}
fn failed(error: impl std::fmt::Display) -> RpcError { RpcError::Failed(error.to_string()) }
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    value.get(key).and_then(Value::as_str).filter(|v| !v.trim().is_empty())
        .ok_or_else(|| RpcError::BadParams(format!("{key} is required")))
}
fn branch_id(command: &str) -> String {
    let hash = Sha256::digest(command.as_bytes());
    let mut bytes = [0; 16]; bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 15) | 0x80; bytes[8] = (bytes[8] & 63) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerifyImportedCrewSessionInput { logical_session_id: String }
impl VerifyImportedCrewSessionInput {
    fn parse(mut params: Value) -> Result<Self, RpcError> {
        if let Some(params) = params.as_object_mut() {
            params.remove("targetDeviceId");
            params.remove("__crewAuthority");
        }
        let input: Self = zeron_rpc::parse_params(params)?;
        if !uuid::Uuid::parse_str(&input.logical_session_id)
            .is_ok_and(|id| id.to_string() == input.logical_session_id) {
            return Err(RpcError::BadParams("logicalSessionId must be a canonical UUID".into()));
        }
        Ok(input)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VerifyImportedCrewSessionResult {
    report: zeron_sync::crew_import::CrewImportReport,
    original_epoch_history_proof_json: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HandoffInput {
    source_chat_id: String,
    command_id: String,
    prompt: String,
    database_environment: ScaffoldDatabaseEnvironment,
    #[serde(default)] session_only: bool,
    #[serde(default)] recover_chat_id: Option<String>,
    #[serde(default)] recover_sandbox_id: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct BranchIntent {
    source: Chat, native_id: String, cwd: String,
    transcript: Vec<zeron_doc::SessionMessageEntry>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerMessage {
    source_chat_id: String,
    #[serde(default)] target_chat_id: String,
    #[serde(default)] command_id: String,
    thread_id: String,
    #[serde(default)] reply_to: Option<String>,
    #[serde(default)] hop_count: u8,
    source_device_id: String,
    #[serde(default)] source_deployment_id: Option<String>,
    text: String,
}
const PEER_PREFIX: &str = "Crew peer message: ";
fn peer_prompt(message: &PeerMessage) -> Result<String, RpcError> {
    Ok(format!("{PEER_PREFIX}{}\n\n{}\n\nReply only if needed: comet session reply --session {} --command '{}' <text>",
        serde_json::to_string(message).map_err(failed)?, message.text, message.target_chat_id, message.command_id.replace('\'', "'\\''")))
}

fn validate_peer(message: &PeerMessage) -> Result<(), RpcError> {
    if message.source_chat_id == message.target_chat_id || message.text.trim().is_empty()
        || message.text.len() > 256 * 1024 || message.hop_count > 8
        || message.command_id.is_empty() || message.command_id.len() > 256
        || message.thread_id.is_empty() || message.thread_id.len() > 256
        || message.source_device_id.is_empty() || message.source_device_id.len() > 256
        || message.reply_to.as_ref().is_some_and(|id| id.is_empty() || id.len() > 256)
        || uuid::Uuid::parse_str(&message.source_chat_id).is_err()
        || uuid::Uuid::parse_str(&message.target_chat_id).is_err() {
        return Err(RpcError::BadParams("Invalid or out-of-bounds peer command".into()));
    }
    Ok(())
}
fn peer_from_payload(payload: &SessionCommandPayload) -> Option<PeerMessage> {
    let prompt = match payload {
        SessionCommandPayload::Run { request, .. } => &request.prompt,
        _ => return None,
    };
    serde_json::from_str(prompt.lines().next()?.strip_prefix(PEER_PREFIX)?).ok()
}
fn request_for(chat: &Chat, prompt: String) -> Result<RunRequest, RpcError> {
    let config = chat.config.as_ref().ok_or_else(|| failed("Session has no stored harness/model configuration"))?;
    if config.model.as_deref().is_none_or(str::is_empty) { return Err(failed("Session requires an explicit stored model")); }
    Ok(RunRequest { prompt, harness: Some(config.harness), model: config.model.clone(),
        reasoning: config.reasoning, model_options: config.model_options.clone(),
        cwd: chat.cwd.clone().or_else(|| chat.harness_session_cwd.clone()).ok_or_else(|| failed("Session has no working directory"))?,
        sandbox: config.sandbox, auto_approve: false, resume: None, attachments: vec![], worktree: None, mcp: None })
}

impl EngineRpc {
    fn native_controller(&self) -> Result<(), RpcError> {
        self.crew_services()?;
        if self.relay || self.auth()?.device_mode() { return Err(failed("Native branch preparation and backup import require local controller IPC")); }
        Ok(())
    }

    fn native_source(&self, id: &str) -> Result<BranchIntent, RpcError> {
        let source = self.crew_services()?.adopt_chat(&self.workspace, id).map_err(failed)?;
        self.sessions.ensure_execution_allowed(id, source.harness_session_id.as_deref()).map_err(failed)?;
        if source.device_id != self.doc_host.device_id() { return Err(failed("Native source must be hosted on this engine")); }
        if !matches!(source.config.as_ref().map(|config| config.harness), Some(HarnessId::Omp | HarnessId::ClaudeCode | HarnessId::Codex)) {
            return Err(failed("This harness has no supported native fork; no transcript-only fallback is permitted"));
        }
        request_for(&source, String::new())?;
        let native_id = source.harness_session_id.clone().filter(|id| !id.is_empty())
            .ok_or_else(|| failed("Source has no durable native identity"))?;
        let cwd = source.harness_session_cwd.clone().filter(|cwd| !cwd.is_empty())
            .ok_or_else(|| failed("Source has no durable native working directory"))?;
        if source.cwd.as_deref().is_some_and(|path| path != cwd
            && !std::fs::canonicalize(path).is_ok_and(|a| std::fs::canonicalize(&cwd).is_ok_and(|b| a == b))) {
            return Err(failed("Source working directory differs from its native journal"));
        }
        let mut transcript = self.doc_host.open(id).map_err(failed)?.doc().read_entries().map_err(failed)?;
        let count = transcript.iter().rposition(|entry| entry.role == zeron_doc::MessageRole::Assistant
            && entry.status == Some(zeron_doc::MessageStatus::Complete)).map_or(0, |index| index + 1);
        transcript.truncate(count);
        Ok(BranchIntent { source, native_id, cwd, transcript })
    }

    async fn prepare_native_branch(&self, key: &str, source_id: &str) -> Result<(BranchIntent, String, String), RpcError> {
        self.sessions.ensure_execution_allowed(source_id, None).map_err(failed)?;
        let store = self.doc_host.crew_store();
        let intent_key = format!("{key}:snapshot");
        let intent: BranchIntent = match store.crew_command(&intent_key).map_err(failed)? {
            Some((bytes, _)) => serde_json::from_slice(&bytes).map_err(failed)?,
            None => {
                let intent = self.native_source(source_id)?;
                store.claim_crew_command(&intent_key, &serde_json::to_vec(&intent).map_err(failed)?).map_err(failed)?;
                intent
            }
        };
        self.sessions.ensure_execution_allowed(source_id, Some(&intent.native_id)).map_err(failed)?;
        if intent.source.config.as_ref().map(|config| config.harness) != Some(HarnessId::Omp) {
            return Err(failed("Scaffold native transfer requires an OMP source"));
        }
        if intent.source.id != source_id { return Err(failed("Native snapshot source is immutable")); }
        let id = branch_id(key);
        let path = self.sessions.native_branch_path(&id).map_err(failed)?;
        let prepared = store.crew_command(&intent_key).map_err(failed)?.is_some_and(|(_, outcome)| outcome.is_some());
        if prepared && !path.is_file() {
            return Err(failed("Prepared native snapshot is missing; refusing to replace it with later history"));
        }
        let selector = self.sessions.native_launch_selector(&intent.native_id);
        let expected_cwd = intent.cwd.clone();
        let expected_id = id.clone();
        let path_string = path.to_string_lossy().into_owned();
        let scaffold_host = self.auth.as_ref().is_some_and(|auth| auth.device_mode());
        let captured = tokio::task::spawn_blocking(move || {
            if !path.exists() {
                zeron_harness::omp::fork_native_session(&selector, &path, &expected_id, scaffold_host).map_err(failed)?;
            }
            if !prepared {
                let blobs = zeron_harness::omp::native_session_blob_dir(&selector, scaffold_host).map_err(failed)?;
                crate::omp_session_artifact::prepare_native_branch_attachments(
                    &path, &expected_id, &expected_cwd, &blobs).map_err(failed)?;
            }
            let captured = crate::omp_session_artifact::capture_omp_file_for_session(
                &path.to_string_lossy(), &expected_cwd, &CancellationToken::new()).map_err(failed)?;
            if captured.native_session_id != expected_id { return Err(failed("Prepared branch identity changed")); }
            Ok::<_, RpcError>(json!({"nativeId":expected_id,"sha256":captured.sha256,"bytes":captured.byte_count}))
        }).await.map_err(failed)??;
        // A retry verifies the exact saved snapshot; it never recaptures a later source turn.
        store.finish_crew_command(&intent_key, &serde_json::to_vec(&captured).map_err(failed)?).map_err(failed)?;
        Ok((intent, id, path_string))
    }

    pub(super) async fn native_rpc(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
        match method {
            "VerifyImportedCrewSession" | "PublishImportedCrewSession" => {
                let (subject, scope) = self.crew_services()?.controller()?;
                if scope.session_id.is_some() {
                    return Err(failed("Imported profile verification requires owner controller authority"));
                }
                let input = VerifyImportedCrewSessionInput::parse(params)?;
                let principal = zeron_proto::CollaborationPrincipal {
                    subject: subject.clone(), email: self.auth()?.verified_owner_email(),
                    project_id: scope.project_id.clone(), deployment_id: scope.deployment_id.clone(),
                    session_id: None, capabilities: vec![], unknown: Default::default(),
                };
                let store = self.doc_host.crew_store();
                let device = self.doc_host.device_id().to_owned();
                let expected_device = device.clone();
                let report = tokio::task::spawn_blocking(move || {
                    let profile = store.profile_root().map_err(failed)?;
                    let report = zeron_sync::crew_import::verify_imported_profile(
                        &profile, &principal, &device, &input.logical_session_id).map_err(failed)?;
                    let proof_json = zeron_sync::crew_import::imported_original_epoch_proof_json(&profile, &report).map_err(failed)?;
                    Ok::<_, RpcError>((report, proof_json))
                }).await.map_err(failed)?;
                let (current_subject, current_scope) = self.crew_services()?.controller()?;
                if current_subject != subject || current_scope != scope
                    || self.doc_host.device_id() != expected_device {
                    return Err(failed("Crew controller scope changed during imported profile verification"));
                }
                let (report, original_epoch_history_proof_json) = report?;
                if method == "PublishImportedCrewSession" {
                    self.doc_host.publish_imported_terminal_history(&report).await.map_err(failed)?;
                    let (current_subject, current_scope) = self.crew_services()?.controller()?;
                    if current_subject != subject || current_scope != scope || self.doc_host.device_id() != expected_device {
                        return Err(failed("Crew controller scope changed during imported history publication"));
                    }
                }
                RpcReply::value(&VerifyImportedCrewSessionResult { report, original_epoch_history_proof_json })
            }
            "ImportCrewBackup" | "ExportCrewBackup" => {
                self.native_controller()?;
                let auth = self.auth()?;
                let scope = auth.verified_scope().ok_or_else(|| failed("Verified scope required"))?;
                let principal = zeron_proto::CollaborationPrincipal {
                    subject: auth.user_id().ok_or_else(|| failed("Verified owner required"))?,
                    email: auth.verified_owner_email(), project_id: scope.project_id,
                    deployment_id: scope.deployment_id, session_id: scope.session_id,
                    capabilities: vec![], unknown: Default::default(),
                };
                if method == "ExportCrewBackup" {
                    if params.get("writersStopped").and_then(Value::as_bool) != Some(true) {
                        return Err(RpcError::BadParams("explicit stopped-writer confirmation required".into()));
                    }
                    let original = std::path::PathBuf::from(string(&params, "sourceProfileRoot")?);
                    let copied = std::path::PathBuf::from(string(&params, "sourceBackupRoot")?);
                    let database = std::path::PathBuf::from(string(&params, "databaseRelative")?);
                    let destination = std::path::PathBuf::from(string(&params, "destination")?);
                    let native_roots: Vec<std::path::PathBuf> = serde_json::from_value(params.get("nativeHistoryRoots")
                        .cloned().unwrap_or_else(|| json!([]))).map_err(|_| RpcError::BadParams("nativeHistoryRoots must be paths".into()))?;
                    let context: Option<zeron_sync::crew_desktop_backup::DesktopSourceContext> = serde_json::from_value(
                        params.get("desktopSourceContext").cloned().unwrap_or(Value::Null)).map_err(|_| RpcError::BadParams("invalid desktop source context".into()))?;
                    let storage: Option<std::path::PathBuf> = serde_json::from_value(params.get("sourceStorageDirectory").cloned().unwrap_or(Value::Null))
                        .map_err(|_| RpcError::BadParams("sourceStorageDirectory must be a path".into()))?;
                    let attachments: Option<std::path::PathBuf> = serde_json::from_value(params.get("attachmentRoot").cloned().unwrap_or(Value::Null))
                        .map_err(|_| RpcError::BadParams("attachmentRoot must be a path".into()))?;
                    let terminal_authority: Option<std::path::PathBuf> = serde_json::from_value(params.get("terminalAuthorityRoot").cloned().unwrap_or(Value::Null))
                        .map_err(|_| RpcError::BadParams("terminalAuthorityRoot must be a path".into()))?;
                    let inventory_only: bool = serde_json::from_value(params.get("inventoryAttachmentsOnly").cloned().unwrap_or(Value::Bool(false)))
                        .map_err(|_| RpcError::BadParams("inventoryAttachmentsOnly must be a Boolean".into()))?;
                    if context.is_some() != storage.is_some()
                        || (context.is_none() && (attachments.is_some() || terminal_authority.is_some() || inventory_only)) {
                        return Err(RpcError::BadParams("desktop source context and storage are required together for witnessed export".into()));
                    }
                    if [&original, &copied, &destination].into_iter().chain(native_roots.iter())
                        .chain(storage.iter()).chain(attachments.iter()).chain(terminal_authority.iter()).any(|path| !path.is_absolute()) {
                        return Err(RpcError::BadParams("source and destination roots must be absolute paths".into()));
                    }
                    let manifest = tokio::task::spawn_blocking(move || -> Result<Value, zeron_sync::crew_import::CrewImportError> {
                        let manifest = if let (Some(context), Some(storage)) = (context, storage) {
                            if inventory_only {
                                return Ok(serde_json::to_value(zeron_sync::crew_backup::inventory_desktop_crew_backup(
                                    &original, &copied, &database, &destination, &native_roots, &principal, &context,
                                    &storage, attachments.as_deref(), terminal_authority.as_deref())?)?);
                            }
                            zeron_sync::crew_backup::export_desktop_crew_backup(&original, &copied, &database, &destination,
                                &native_roots, &principal, &context, &storage, attachments.as_deref(), terminal_authority.as_deref())?
                        } else {
                            zeron_sync::crew_backup::export_legacy_crew_backup(&original, &copied, &database, &destination, &native_roots, &principal)?
                        };
                        Ok(serde_json::to_value(manifest)?)
                    })
                        .await.map_err(failed)?.map_err(failed)?;
                    return RpcReply::value(&manifest);
                }
                let source = std::path::PathBuf::from(string(&params, "backup")?);
                let destination = std::path::PathBuf::from(string(&params, "destination")?);
                if !source.is_absolute() || !destination.is_absolute() { return Err(failed("Backup and new destination must be absolute paths")); }
                let device = self.doc_host.device_id().to_owned();
                let validate_only = match params.get("validateOnly") {
                    None => false,
                    Some(Value::Bool(value)) => *value,
                    Some(_) => return Err(RpcError::BadParams("validateOnly must be a boolean".into())),
                };
                let verify_only = match params.get("verifyOnly") {
                    None => false,
                    Some(Value::Bool(value)) => *value,
                    Some(_) => return Err(RpcError::BadParams("verifyOnly must be a boolean".into())),
                };
                if validate_only && verify_only { return Err(RpcError::BadParams("validation and destination readback are separate modes".into())); }
                let report = tokio::task::spawn_blocking(move || {
                    if validate_only {
                        zeron_sync::crew_import::preflight_legacy_crew_backup(&source, &destination, &principal, &device)
                    } else {
                        let import = if verify_only { zeron_sync::crew_import::verify_legacy_crew_import }
                            else { zeron_sync::crew_import::import_legacy_crew_backup };
                        import(&source, &destination, &principal, &device)
                            .and_then(|report| serde_json::to_value(report).map_err(Into::into))
                    }
                }).await.map_err(failed)?.map_err(failed)?;
                RpcReply::value(&report)
            }
            "ForkSession" => {
                self.native_controller()?;
                let _gate = NATIVE_GATE.lock().await;
                let source_id = string(&params, "sourceChatId")?;
                self.sessions.ensure_execution_allowed(source_id, None).map_err(failed)?;
                let command = string(&params, "commandId")?;
                if command.len() > 256 { return Err(RpcError::BadParams("commandId exceeds 256 bytes".into())); }
                let key = format!("native-fork:{command}");
                let store = self.doc_host.crew_store();
                store.claim_crew_command(&key, &serde_json::to_vec(&params).map_err(failed)?).map_err(failed)?;
                if let Some((_, Some(receipt))) = store.crew_command(&key).map_err(failed)? {
                    return RpcReply::value(&serde_json::from_slice::<Value>(&receipt).map_err(failed)?);
                }
                let seed_key = format!("{key}:source");
                let seed: BranchIntent = match store.crew_command(&seed_key).map_err(failed)? {
                    Some((bytes, _)) => serde_json::from_slice(&bytes).map_err(failed)?,
                    None => {
                        let seed = self.native_source(source_id)?;
                        store.claim_crew_command(&seed_key, &serde_json::to_vec(&seed).map_err(failed)?).map_err(failed)?;
                        seed
                    }
                };
                self.sessions.ensure_execution_allowed(source_id, Some(&seed.native_id)).map_err(failed)?;
                let native_fork_pending = seed.source.config.as_ref().map(|config| config.harness) != Some(HarnessId::Omp);
                let (intent, id) = if native_fork_pending {
                    let id = seed.native_id.clone();
                    (seed, id)
                } else {
                    let (intent, id, _) = self.prepare_native_branch(&key, source_id).await?;
                    (intent, id)
                };
                let mut chat = intent.source.clone();
                chat.id = branch_id(&format!("{key}:chat")); chat.parent_chat_id = Some(source_id.into());
                if native_fork_pending {
                    store.claim_crew_command(&format!("native-fork-source:{}", chat.id),
                        &serde_json::to_vec(&json!({"source":intent.native_id,"harness":chat.config.as_ref().map(|config| config.harness)})).map_err(failed)?)
                        .map_err(failed)?;
                }
                chat.title = None; chat.archived = false; chat.created_at = chrono::Utc::now();
                chat.last_message_at = None; chat.last_message_preview = None; chat.last_seen_at = None;
                chat.harness_session_id = Some(id); chat.harness_session_cwd = Some(intent.cwd.clone());
                chat.cwd = Some(intent.cwd); chat.room_gen = Some(2);
                chat.crew = Some(self.crew_services()?.session_metadata(&chat.id).map_err(failed)?);
                self.copy_native_transcript(&intent.transcript, &chat.id)?;
                self.workspace.import_chat_row(&chat).map_err(failed)?;
                let receipt = json!({"chatId":chat.id,"commandId":command,"nativeSessionId":chat.harness_session_id,"nativeForkPending":native_fork_pending});
                store.finish_crew_command(&key, &serde_json::to_vec(&receipt).map_err(failed)?).map_err(failed)?;
                RpcReply::value(&receipt)
            }
            "HandoffSessionToScaffold" | "RecoverSessionHandoffToScaffold"
            | "HandoffSessionOnlyToScaffold" | "RecoverSessionOnlyHandoffToScaffold" => {
                self.native_controller()?;
                let _gate = NATIVE_GATE.lock().await;
                let mut input: HandoffInput = zeron_rpc::parse_params(params)?;
                let session_only = matches!(method, "HandoffSessionOnlyToScaffold" | "RecoverSessionOnlyHandoffToScaffold");
                if input.session_only && !session_only {
                    return Err(failed("Session-only transfer requires its dedicated RPC method"));
                }
                input.session_only = session_only;
                if matches!(method, "RecoverSessionHandoffToScaffold" | "RecoverSessionOnlyHandoffToScaffold") {
                    input = self.recover_native_input(input)?;
                } else if input.recover_chat_id.is_some() || input.recover_sandbox_id.is_some() {
                    return Err(failed("Preserved targets require the recovery method"));
                }
                if input.prompt.trim().is_empty() || input.prompt.len() > 1024 * 1024 || input.command_id.is_empty() || input.command_id.len() > 256 {
                    return Err(RpcError::BadParams("Nonempty bounded prompt and immutable commandId required".into()));
                }
                let key = format!("native-handoff:{}", input.command_id);
                let store = self.doc_host.crew_store();
                store.claim_crew_command(&key, &serde_json::to_vec(&input).map_err(failed)?).map_err(failed)?;
                let (intent, _, selector) = self.prepare_native_branch(&key, &input.source_chat_id).await?;
                let config = intent.source.config.as_ref().ok_or_else(|| failed("Missing source model"))?;
                let mut route = AgentRoute::from_omp_model(config.model.as_deref().unwrap_or_default())
                    .ok_or_else(|| failed("Native handoff requires an explicit provider-qualified OMP model"))?;
                if let Some(account) = config.model_options.get("crewAccountId").and_then(Value::as_str) {
                    route.account_id = Some(account.into()); route.routing_mode = zeron_proto::AgentRoutingMode::Pinned;
                }
                let receipt = self.crew_services()?.prepare_handoff(crate::crew::HandoffRequest {
                    command_id: input.command_id.clone(), source_session_id: input.source_chat_id.clone(),
                    native_session_id: selector, cwd: intent.cwd, prompt: input.prompt.clone(),
                    agent_route: route.clone(), database_environment: input.database_environment,
                    session_only: input.session_only,
                    recover_chat_id: input.recover_chat_id, recover_sandbox_id: input.recover_sandbox_id,
                }, &CancellationToken::new()).await?;
                let mut chat = self.publish_crew_environment(receipt.attachment.clone(), Some(route)).await?;
                chat.config = intent.source.config.clone(); chat.cwd = Some(receipt.cwd.clone());
                chat.source_context = intent.source.source_context.clone();
                chat.harness_session_id = Some(receipt.native_session_id.clone());
                chat.harness_session_cwd = Some(receipt.cwd.clone());
                self.workspace.import_chat_row(&chat).map_err(failed)?;
                self.copy_native_transcript(&intent.transcript, &chat.id)?;
                let payload = SessionCommandPayload::Run { request: request_for(&chat, input.prompt)?, message_id: input.command_id.clone() };
                let host = self.doc_host.clone();
                let command = input.command_id.clone();
                let chat_id = chat.id.clone();
                self.crew_services()?.admit_handoff(&receipt, move || {
                    host.queue_command_named(&chat_id, payload, vec![], Some(&command), None).map_err(failed)
                }).await?;
                store.finish_crew_command(&key, &serde_json::to_vec(&receipt).map_err(failed)?).map_err(failed)?;
                RpcReply::value(&receipt)
            }
            "DeliverPeerMessage" => {
                let rpc = self.clone();
                let receipt = tokio::task::spawn_blocking(move || rpc.deliver_native_peer(params)).await.map_err(failed)??;
                RpcReply::value(&receipt)
            }
            "SendPeerMessage" | "ReplyPeerMessage" => {
                self.native_peer(method, params).await
            }
            "WaitPeerReply" => {
                let source = string(&params, "sourceChatId")?;
                self.native_peer_source(source)?;
                let registration = PeerWaitGuard::register(self.native_wait_key(source, string(&params,"threadId")?)?, None)?;
                let result = self.wait_native_peer(registration, params.get("timeoutMs").and_then(Value::as_u64)).await?;
                RpcReply::value(&json!({"threadId":string(&params,"threadId")?,"reply":result}))
            }
            _ => Err(RpcError::UnknownMethod(method.into())),
        }
    }

    fn recover_native_input(&self, requested: HandoffInput) -> Result<HandoffInput, RpcError> {
        let chat = requested.recover_chat_id.as_deref().ok_or_else(|| failed("Recovery requires chat ID"))?;
        let sandbox = requested.recover_sandbox_id.as_deref().ok_or_else(|| failed("Recovery requires sandbox ID"))?;
        let store = self.doc_host.crew_store();
        for (key, _, outcome) in store.crew_commands().map_err(failed)? {
            if !key.starts_with("crew-handoff:") { continue; }
            let Some(outcome) = outcome else { continue };
            let Ok(receipt) = serde_json::from_slice::<crate::crew::HandoffReceipt>(&outcome) else { continue };
            if receipt.chat_id != chat || receipt.sandbox_id != sandbox { continue; }
            let (bytes, _) = store.crew_command(&format!("native-handoff:{}", receipt.command_id)).map_err(failed)?
                .ok_or_else(|| failed("Native recovery intent unavailable"))?;
            let original: HandoffInput = serde_json::from_slice(&bytes).map_err(failed)?;
            if original.source_chat_id != requested.source_chat_id || original.prompt != requested.prompt
                || original.database_environment != requested.database_environment
                || original.session_only != requested.session_only
                || (!requested.command_id.is_empty() && original.command_id != requested.command_id) {
                return Err(failed("Recovery cannot change the caller command, source, prompt, database or transfer mode"));
            }
            // prepare_handoff revalidates Stop/owner/scope before the same durable
            // prompt admission; never transfer another native branch to this target.
            return Ok(original);
        }
        Err(failed("No durable native preparation receipt matches those target IDs. Outcome may be unknown; reconcile the existing operation, never create a replacement"))
    }

    fn copy_native_transcript(&self, entries: &[zeron_doc::SessionMessageEntry], target: &str) -> Result<(), RpcError> {
        let target = self.doc_host.open(target).map_err(failed)?;
        let existing = target.doc().read_entries().map_err(failed)?;
        if let Some(boundary) = entries.iter().rposition(|entry| entry.role == zeron_doc::MessageRole::Assistant
            && entry.status == Some(zeron_doc::MessageStatus::Complete)) {
            for entry in &entries[..=boundary] {
                if existing.iter().any(|known| known.id == entry.id) { continue; }
                let mut entry = entry.clone();
                for part in &mut entry.parts {
                    if let zeron_doc::MessagePart::Input { resolved, .. } = part { *resolved = true; }
                }
                target.doc().push_message(&entry).map_err(failed)?;
            }
        }
        self.doc_host.persist_fork(&target).map_err(failed)
    }

    fn native_peer_source(&self, id: &str) -> Result<Chat, RpcError> {
        let chat = self.crew_services()?.adopt_chat(&self.workspace, id).map_err(failed)?;
        if chat.device_id != self.doc_host.device_id() { return Err(failed("Peer source must be hosted by this engine")); }
        Ok(chat)
    }

    fn native_wait_key(&self, source: &str, thread: &str) -> Result<WaitKey, RpcError> {
        if thread.is_empty() || thread.len() > 256 { return Err(failed("Invalid peer thread ID")); }
        Ok((self.auth()?.verified_profile_key().ok_or_else(|| failed("Verified peer profile unavailable"))?,
            source.into(), thread.into()))
    }

    fn queue_native_peer(&self, message: &PeerMessage) -> Result<Value, RpcError> {
        let _admission = PEER_ADMISSION.lock();
        validate_peer(message)?;
        let target = self.crew_services()?.adopt_chat(&self.workspace, &message.target_chat_id).map_err(failed)?;
        if target.device_id != self.doc_host.device_id() { return Err(failed("Peer target is not owned by this engine")); }
        let handle = self.doc_host.open(&target.id).map_err(failed)?;
        let existing = handle.doc().read_commands().map_err(failed)?.into_iter().find(|entry| entry.id == message.command_id);
        let previously_admitted = existing.is_some();
        let payload = if let Some(existing) = existing {
            if peer_from_payload(&existing.payload).as_ref() != Some(message) {
                return Err(failed("Peer command ID has a different immutable sender or message"));
            }
            existing.payload
        } else {
            SessionCommandPayload::Run {
                request: request_for(&target, peer_prompt(message)?)?, message_id: message.command_id.clone(),
            }
        };
        let waiter = if message.reply_to.is_some() {
            let key = self.native_wait_key(&message.target_chat_id, &message.thread_id)?;
            let mut waits = PEER_WAITS.lock();
            if waits.get(&key).is_some_and(|owner| owner.after.as_deref().is_none_or(|id| message.reply_to.as_deref() == Some(id))) {
                waits.remove(&key)
            } else { None }
        } else { None };
        if waiter.is_some() && !previously_admitted {
            // A reply consumed by the waiting CLI is not a second agent prompt.
            // Claim before append, preserving upstream's no-replay crash rule.
            self.doc_host.crew_store().mark_processed(&message.command_id).map_err(failed)?;
            handle.doc().queue_command(&zeron_doc::SessionCommandEntry {
                id:message.command_id.clone(), payload, issued_by:self.doc_host.device_id().into(),
                issued_at:crate::now_ms(), based_on:None, expires_at:None,
                status:zeron_doc::SessionCommandStatus::Applied, resolution:Some("Delivered to waiting CLI".into()),
            }).map_err(failed)?;
            handle.write_user_message(&message.command_id, &peer_prompt(message)?, crate::now_ms()).map_err(failed)?;
            self.doc_host.persist_fork(&handle).map_err(failed)?;
        } else {
            self.doc_host.queue_command_named(&target.id, payload, vec![], Some(&message.command_id), None).map_err(failed)?;
        }
        if let Some(waiter) = waiter {
            let _ = waiter.sender.send(json!({"commandId":message.command_id,"sourceChatId":message.source_chat_id,"text":message.text}));
        }
        Ok(json!({"commandId":message.command_id,"threadId":message.thread_id,"status":"accepted","delivery":"pending"}))
    }

    fn deliver_native_peer(&self, params: Value) -> Result<Value, RpcError> {
        if !self.relay { return Err(failed("Directional peer admission is relay-only")); }
        let command = params.get("command").ok_or_else(|| failed("Missing peer command"))?;
        if command["kind"].as_str() != Some("peerMessage") { return Err(failed("Directional link accepts peer messages only")); }
        let mut message: PeerMessage = serde_json::from_value(command.clone()).map_err(failed)?;
        message.command_id = string(&params, "commandId")?.into();
        message.target_chat_id = string(&params, "chatId")?.into();
        validate_peer(&message)?;
        let authority = params.get("__crewAuthority").ok_or_else(|| failed("Missing verified peer authority"))?;
        let peer = authority.get("peer").ok_or_else(|| failed("Missing verified directional peer binding"))?;
        if peer["sourceSessionId"].as_str() != Some(message.source_chat_id.as_str())
            || peer["sourceDeviceId"].as_str() != Some(message.source_device_id.as_str())
            || peer["targetSessionId"].as_str() != Some(message.target_chat_id.as_str())
            || peer["targetDeviceId"].as_str() != Some(self.doc_host.device_id())
            || authority["deploymentId"].as_str() != message.source_deployment_id.as_deref()
            || authority.get("sessionId").and_then(Value::as_str).is_some_and(|id| id != message.source_chat_id)
            || authority.get("deviceId").and_then(Value::as_str).is_some_and(|id| id != message.source_device_id) {
            return Err(failed("Peer command does not match its verified directional authority"));
        }
        self.queue_native_peer(&message)
    }

    async fn native_peer(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
        if self.relay { return Err(failed("Send/reply originate on local IPC; relay uses directional admission only")); }
        let command = params.get("sendCommandId").or_else(|| if method == "SendPeerMessage" { params.get("commandId") } else { None })
            .and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| {
                if method == "ReplyPeerMessage" { format!("reply:{}", params["commandId"].as_str().unwrap_or_default()) }
                else { crate::new_id() }
            });
        let text = string(&params, "text")?.to_owned();
        let scope = self.auth()?.verified_scope().ok_or_else(|| failed("Verified scope required"))?;
        let source_device = self.doc_host.device_id().to_owned();
        let (message, reply_device, target_deployment) = if method == "ReplyPeerMessage" {
            let source = string(&params, "sessionId")?;
            self.native_peer_source(source)?;
            let handle = self.doc_host.open(source).map_err(failed)?;
            let inbound = handle.doc().read_commands().map_err(failed)?.into_iter()
                .find(|entry| entry.id == string(&params, "commandId").unwrap_or_default())
                .and_then(|entry| peer_from_payload(&entry.payload)).ok_or_else(|| failed("Inbound peer command is unavailable"))?;
            if inbound.target_chat_id != source || inbound.hop_count >= 8 { return Err(failed("Invalid peer recipient or reply hop limit")); }
            (PeerMessage { source_chat_id: source.into(), target_chat_id: inbound.source_chat_id,
                command_id: command, thread_id: inbound.thread_id, reply_to: Some(inbound.command_id), hop_count: inbound.hop_count + 1,
                source_device_id:source_device, source_deployment_id:scope.deployment_id.clone(), text },
                Some(inbound.source_device_id), inbound.source_deployment_id)
        } else {
            (PeerMessage { source_chat_id: string(&params,"sourceChatId")?.into(), target_chat_id: string(&params,"targetChatId")?.into(),
                command_id: command.clone(), thread_id: command, reply_to: None, hop_count: 0,
                source_device_id:source_device, source_deployment_id:scope.deployment_id.clone(), text }, None, scope.deployment_id.clone())
        };
        validate_peer(&message)?;
        self.native_peer_source(&message.source_chat_id)?;
        if target_deployment != scope.deployment_id { return Err(failed("Peer reply crosses deployment scope")); }
        let timeout = params.get("timeoutMs").and_then(Value::as_u64);
        if timeout.is_some_and(|ms| ms > 120_000) { return Err(failed("Reply timeout may not exceed 120000ms")); }
        let registration = if params.get("wait").and_then(Value::as_bool).unwrap_or(false) {
            Some(PeerWaitGuard::register(self.native_wait_key(&message.source_chat_id, &message.thread_id)?, Some(&message.command_id))?)
        } else { None };
        let store = self.doc_host.crew_store();
        let key = format!("native-peer:{}", message.command_id);
        store.claim_crew_command(&key, &serde_json::to_vec(&message).map_err(failed)?).map_err(failed)?;
        let mut result = if let Some((_, Some(bytes))) = store.crew_command(&key).map_err(failed)? {
            serde_json::from_slice::<Value>(&bytes).map_err(failed)?
        } else {
            let local = self.workspace.chat(&message.target_chat_id).map_err(failed)?
                .is_some_and(|chat| chat.device_id == self.doc_host.device_id());
            let receipt = if local {
                let rpc = self.clone(); let message = message.clone();
                tokio::task::spawn_blocking(move || rpc.queue_native_peer(&message)).await.map_err(failed)??
            } else {
                let links = self.links.as_ref().ok_or_else(|| failed("Directional peer relay is unavailable; retain the command ID"))?;
                let wire = json!({"chatId":message.target_chat_id,"commandId":message.command_id,"command":{
                    "kind":"peerMessage","text":message.text,"sourceChatId":message.source_chat_id,
                    "sourceDeploymentId":message.source_deployment_id,"sourceDeviceId":message.source_device_id,
                    "threadId":message.thread_id,"replyTo":message.reply_to,"hopCount":message.hop_count}});
                if let Some(device) = reply_device {
                    links.peer_device_call(&device, &message.target_chat_id, target_deployment.as_deref(), "DeliverPeerMessage", wire).await?
                } else {
                    links.peer_call(&message.target_chat_id, target_deployment.as_deref(), "DeliverPeerMessage", wire).await?
                }
            };
            if receipt["commandId"].as_str() != Some(message.command_id.as_str()) {
                return Err(failed("Peer admission outcome unknown; retry only the original command ID"));
            }
            store.finish_crew_command(&key, &serde_json::to_vec(&receipt).map_err(failed)?).map_err(failed)?;
            receipt
        };
        if let Some(registration) = registration {
            result["reply"] = self.wait_native_peer(registration, timeout).await?;
        }
        RpcReply::value(&result)
    }

    async fn wait_native_peer(&self, mut registration: PeerWaitGuard, timeout: Option<u64>) -> Result<Value, RpcError> {
        let timeout = timeout.unwrap_or(120_000);
        if timeout > 120_000 { return Err(RpcError::BadParams("Reply timeout may not exceed 120000ms".into())); }
        match tokio::time::timeout(std::time::Duration::from_millis(timeout), &mut registration.receiver).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(failed("Peer reply admission failed; inspect the preserved command")),
            Err(_) => Ok(Value::Null),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn imported_profile_verification_authenticates_before_profile_lookup() {
        use zeron_rpc::RpcService;
        let temp = tempfile::tempdir().unwrap();
        let core = crate::EngineCore::assemble_with_profile(
            crate::EngineProfile::local(temp.path()).unwrap(),
            std::sync::Arc::new(crate::HarnessRegistry::new()), HarnessId::Omp, None,
        ).unwrap();
        for rpc in [core.build_rpc(), core.build_rpc().for_relay()] {
            for params in [json!({"logicalSessionId": "53a67c53-cbc2-4a86-bffd-4c9f5c735590"}),
                json!({"logicalSessionId": "invalid", "profileRoot": "/caller/path"})] {
                for method in ["VerifyImportedCrewSession", "PublishImportedCrewSession"] {
                    let error = match rpc.handle(method, params.clone()).await {
                        Err(error) => error,
                        Ok(_) => panic!("unauthenticated imported profile operation succeeded"),
                    };
                    assert!(error.to_string().contains("Crew sign-in required"));
                }
            }
        }
        assert!(core.rpc_service().handle("VerifyImportedCrewSession", json!({
            "logicalSessionId": "53a67c53-cbc2-4a86-bffd-4c9f5c735590",
            "__crewAuthority": {"subject": "owner"}
        })).await.is_err());
        core.sessions.shutdown().await;
    }

    #[test]
    fn imported_profile_verification_has_a_fixed_session_only_route() {
        let id = "53a67c53-cbc2-4a86-bffd-4c9f5c735590";
        assert!(handles("VerifyImportedCrewSession"));
        assert!(super::super::forwardable("VerifyImportedCrewSession"));
        assert!(handles("PublishImportedCrewSession"));
        assert!(super::super::forwardable("PublishImportedCrewSession"));
        assert!(VerifyImportedCrewSessionInput::parse(json!({"logicalSessionId": id})).is_ok());
        assert!(VerifyImportedCrewSessionInput::parse(json!({"logicalSessionId": id,
            "targetDeviceId": "host", "__crewAuthority": {}})).is_ok());
        for extra in ["backup", "source", "destination", "profileRoot", "principal", "report", "chatId"] {
            let mut params = json!({"logicalSessionId": id});
            params[extra] = json!("caller-controlled");
            assert!(VerifyImportedCrewSessionInput::parse(params).is_err());
        }
        for invalid in ["", "not-a-uuid", "53A67C53-CBC2-4A86-BFFD-4C9F5C735590", "53a67c53cbc24a86bffd4c9f5c735590"] {
            assert!(VerifyImportedCrewSessionInput::parse(json!({"logicalSessionId": invalid})).is_err());
        }
    }
    #[test]
    fn immutable_native_branch_and_peer_admission_identity() {
        assert_eq!(branch_id("caller-1"), branch_id("caller-1"));
        assert_ne!(branch_id("caller-1"), branch_id("caller-2"));
        let message = PeerMessage { source_chat_id:"source".into(),target_chat_id:"target".into(),
            command_id:"caller-1".into(),thread_id:"caller-1".into(),reply_to:None,hop_count:0,
            source_device_id:"device".into(),source_deployment_id:Some("canary".into()),text:"Do work".into() };
        let prompt = peer_prompt(&message).unwrap();
        let parsed: PeerMessage = serde_json::from_str(prompt.lines().next().unwrap().strip_prefix(PEER_PREFIX).unwrap()).unwrap();
        assert_eq!(parsed.command_id,"caller-1");
        assert!(prompt.contains("--session target --command 'caller-1'"));
    }

    #[tokio::test]
    async fn restart_admission_retains_caller_id_and_stopped_outcome() {
        use crate::doc_host::{DocHost, DocHostConfig};
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(zeron_sync::DocsStore::open(dir.path()).unwrap());
        let config = DocHostConfig { device_id:"controller".into(), default_harness:HarnessId::Mock, edge:None };
        let host = DocHost::new(store.clone(), config.clone());
        let payload = SessionCommandPayload::Steer { prompt:"remote task".into(), message_id:Some("caller".into()) };
        assert_eq!(host.queue_command_named("target", payload.clone(), vec![], Some("caller"), None).unwrap(), "caller");
        let handle = host.open("target").unwrap();
        handle.doc().set_command_status("caller", zeron_doc::SessionCommandStatus::Cancelled, Some("Stopped")).unwrap();
        host.persist_fork(&handle).unwrap();
        assert!(store.mark_processed("caller").unwrap());
        let reopened = DocHost::new(store.clone(), config);
        assert_eq!(reopened.queue_command_named("target", payload, vec![], Some("caller"), None).unwrap(), "caller");
        let entries = reopened.open("target").unwrap().doc().read_commands().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, zeron_doc::SessionCommandStatus::Cancelled);
        assert!(!store.mark_processed("caller").unwrap());
        assert!(reopened.queue_command_named("target",
            SessionCommandPayload::Steer { prompt:"different task".into(), message_id:Some("caller".into()) },
            vec![], Some("caller"), None).is_err());
    }

    #[tokio::test]
    async fn targeted_interrupt_rejects_stale_admission_and_preserves_retries() {
        use crate::doc_host::{DocHost, DocHostConfig};
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(zeron_sync::DocsStore::open(dir.path()).unwrap());
        let host = DocHost::new(store, DocHostConfig { device_id:"controller".into(), default_harness:HarnessId::Mock, edge:None });
        let handle = host.open("target").unwrap();
        handle.write_user_message("first", "first turn", 1).unwrap();
        let stop = SessionCommandPayload::Interrupt {};
        host.queue_command_named("target", stop.clone(), vec![], Some("stop-first"), Some("first")).unwrap();
        handle.write_user_message("second", "replacement turn", 2).unwrap();
        assert!(host.queue_command_named("target", stop.clone(), vec![], Some("stale"), Some("first")).is_err());
        assert_eq!(host.queue_command_named("target", stop.clone(), vec![], Some("stop-first"), Some("first")).unwrap(), "stop-first");
        assert!(host.queue_command_named("target", stop, vec![], Some("stop-first"), Some("second")).is_err());
        assert!(!handle.doc().read_commands().unwrap().iter().any(|entry| entry.id == "stale"));
    }

    #[tokio::test]
    async fn immediate_reply_is_reserved_once_and_old_wait_cannot_remove_new_wait() {
        let key = ("test-profile".into(), "source".into(), crate::new_id());
        let mut first = PeerWaitGuard::register(key.clone(), Some("request")).unwrap();
        assert!(PeerWaitGuard::register(key.clone(), None).is_err());
        let owner = PEER_WAITS.lock().remove(&key).unwrap();
        owner.sender.send(json!({"commandId":"reply:request","text":"answer"})).unwrap();
        assert_eq!((&mut first.receiver).await.unwrap()["text"], "answer");
        let second = PeerWaitGuard::register(key.clone(), None).unwrap();
        drop(first);
        assert!(PEER_WAITS.lock().contains_key(&key));
        drop(second);
        assert!(!PEER_WAITS.lock().contains_key(&key));
    }
}
