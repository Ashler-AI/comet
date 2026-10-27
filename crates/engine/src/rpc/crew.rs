use super::EngineRpc;
use serde::Deserialize;
use serde_json::{Value, json};
use zeron_proto::{
    AnchorTargetKind, AnnotationCommand, CapabilityGrant, Chat, ChatConfig,
    CollaborationPrincipal, CollaborationScope, CrewSessionLink, Device, FileTargetReference,
    HarnessId, ScaffoldEnvironmentControl, ScaffoldEnvironmentControlResult,
    ScaffoldEnvironmentSnapshot, ScaffoldLifecycle, ScaffoldSessionLink, SemanticAnnotation,
    SessionEnvironmentSource, CAPABILITY_SESSION_ANNOTATE, CAPABILITY_SESSION_READ,
};
use zeron_rpc::{RpcError, RpcReply, methods};

pub(super) fn handles(method: &str) -> bool {
    method.starts_with("scaffold.") || method.starts_with("crew.")
        || matches!(method, "ResolveCrewSessionLink" | "ListCrewAnnotations" | "ApplyCrewAnnotation"
            | "ScaffoldHostAuthority" | "SearchSessions" | "GetHarnessAutoUpdate" | "SetHarnessAutoUpdate")
}

pub(super) fn forwardable(method: &str) -> bool {
    handles(method) && !matches!(method, "crew.accounts.importLocal" | "ScaffoldHostAuthority")
        && !method.starts_with("crew.accounts.login.")
}

fn failed(error: impl std::fmt::Display) -> RpcError { RpcError::Failed(error.to_string()) }
fn denied(message: &str) -> RpcError { RpcError::Failed(message.into()) }

// Match Edge's scopedRpcAllowed contract before touching the shared device link.
// A rejected device-wide watch otherwise tears down unrelated session streams.
fn scaffold_host_rpc_allowed(method: &str, params: &Value, session_id: &str) -> bool {
    if params.get("chatId").and_then(Value::as_str) != Some(session_id) { return false; }
    match method {
        methods::WATCH_DOC_MESSAGES | methods::WATCH_QUEUE | methods::FOCUS_CHAT
        | methods::UPLOAD_CHUNK | methods::UPLOAD_COMMIT
        | "ListCrewAnnotations" | "ApplyCrewAnnotation" => true,
        methods::QUEUE_COMMAND => matches!(params.get("command").and_then(|command| command.get("kind"))
            .and_then(Value::as_str), Some("run" | "steer" | "interrupt" | "respondInput")),
        _ => false,
    }
}

fn own_directory_search(params: &mut Value, owner: &str) -> Result<(), RpcError> {
    let object = params.as_object_mut().ok_or_else(|| denied("Session search requires an object"))?;
    // CLI Option fields arrive as null; the directory API requires omission.
    object.retain(|key, value| !value.is_null()
        || !matches!(key.as_str(), "query" | "sourceUrl" | "ownerId" | "limit" | "cursor"));
    if object.get("ownerId").is_some_and(|value| value.as_str() != Some(owner)) {
        return Err(denied("Personal session search cannot select another owner"));
    }
    object.insert("ownerId".into(), Value::String(owner.into()));
    Ok(())
}

fn validate_start_history(sandbox_id: &str, expected_epoch: u64,
    trusted: Option<&zeron_proto::OriginalEpochHistory>, marker: Option<&zeron_proto::OriginalEpochHistory>) -> Result<(), RpcError> {
    match trusted {
        Some(history) if history.sandbox_id != sandbox_id
            || expected_epoch < history.coordinator_lifecycle_epoch
            || marker.is_some_and(|marker| marker != history) =>
            Err(denied("Start differs from the immutable original epoch")),
        None if marker.is_some() => Err(denied("Original epoch receipt is not trusted by this profile; the control plane exposes bootstrap evidence only to a verified new-epoch host, not to this controller. Start was not sent.")),
        _ => Ok(()),
    }
}

fn bind_scaffold_workspace(chat: &mut Chat, host: &str, sandbox_id: &str, handoff_cwd: Option<String>) {
    let same_sandbox = matches!(chat.crew.as_ref().and_then(|crew| crew.environment.as_ref()).map(|environment| &environment.source),
        Some(SessionEnvironmentSource::Scaffold { sandbox_id: previous, .. }) if previous == sandbox_id);
    if chat.device_id != host && !same_sandbox {
        // A local project path belongs to its old host, not the sandbox. Keep
        // same-sandbox worktrees across epoch changes; handoff receipts win below.
        chat.cwd = None;
        chat.space_id = None;
    }
    chat.device_id = host.to_owned();
    chat.cwd.get_or_insert_with(|| "/workspace/ashler-platform".into());
    if let Some(cwd) = handoff_cwd {
        chat.cwd = Some(cwd.clone());
        chat.harness_session_cwd = Some(cwd);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayAuthority {
    subject: String,
    project_id: String,
    deployment_id: Option<String>,
    session_id: Option<String>,
    device_id: Option<String>,
    grant_id: Option<String>,
    expires_at: Option<i64>,
    lifecycle_epoch: Option<u64>,
    capabilities: Vec<String>,
    peer: Option<PeerAuthority>,
}

impl RelayAuthority {
    fn require_import_controller(&self) -> Result<(), RpcError> {
        if self.session_id.is_some() || self.grant_id.is_some() || self.peer.is_some() {
            return Err(denied("Imported profile verification requires owner controller authority"));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerAuthority {
    source_session_id: String,
    source_device_id: String,
    target_session_id: String,
    target_device_id: String,
}

impl EngineRpc {
    pub(super) fn crew_services(&self) -> Result<&crate::crew::CrewServices, RpcError> {
        let services = self.crew.as_ref().ok_or_else(|| denied("Crew sign-in required"))?;
        services.ensure_identity().map_err(failed)?;
        Ok(services)
    }

    pub(super) fn guard_scaffold_target(&self, target: &str, method: &str, params: &Value) -> Result<(), RpcError> {
        let chats = self.workspace.watch_chats();
        if let Some(chat) = chats.borrow().iter().find(|chat| chat.device_id == target
            && chat.crew.as_ref().and_then(|metadata| metadata.environment.as_ref())
                .is_some_and(|environment| matches!(environment.source, SessionEnvironmentSource::Scaffold { .. })))
            && !scaffold_host_rpc_allowed(method, params, &chat.id)
        {
            return Err(RpcError::Failed(format!("{method} is unavailable for this session-scoped Scaffold host")));
        }
        Ok(())
    }

    pub(super) fn crew_guard(&self, method: &str, params: &Value) -> Result<(), RpcError> {
        if !self.relay && params.get("__crewAuthority").is_some() {
            return Err(denied("Relay authority cannot be supplied over local IPC"));
        }
        if (matches!(method, "crew.accounts.importLocal" | "ScaffoldHostAuthority")
            || method.starts_with("crew.accounts.login."))
            && params.get("targetDeviceId").and_then(Value::as_str).is_some_and(|device| device != self.doc_host.device_id())
        {
            return Err(denied("This operation requires local IPC on the explicitly selected device"));
        }
        if method.starts_with("crew.accounts.login.") {
            if self.relay { return Err(denied("Shared account login requires local controller consent")); }
            self.crew_services()?.controller()?;
        }
        if self.registry.managed_inference() {
            if super::voice_remote::handles(method) {
                return Err(denied("Realtime voice is unavailable in Crew-managed mode"));
            }
            if matches!(method, methods::ACTIVATE_AGENT_ACCOUNT | methods::FORGET_AGENT_ACCOUNT
                | methods::START_AGENT_LOGIN | methods::COMPLETE_AGENT_LOGIN
                | methods::POLL_AGENT_LOGIN | methods::CANCEL_AGENT_LOGIN)
                || (method == methods::LIST_AGENT_ACCOUNTS && params.get("forceUsage").and_then(Value::as_bool) == Some(true))
            {
                return Err(denied("Managed accounts must use Crew shared-account operations; direct provider access is disabled"));
            }
        }
        let Some(auth) = self.auth.as_ref().filter(|auth| auth.scaffold_enabled()) else { return Ok(()) };
        if self.relay {
            let authority: RelayAuthority = serde_json::from_value(params.get("__crewAuthority")
                .cloned().ok_or_else(|| denied("Missing verified relay authority"))?).map_err(failed)?;
            let scope = auth.verified_scope().ok_or_else(|| denied("Crew sign-in required"))?;
            if authority.project_id != scope.project_id || authority.deployment_id != scope.deployment_id {
                return Err(denied("Relay authority belongs to another deployment"));
            }
            if authority.subject != auth.user_id().unwrap_or_default()
                && !(authority.session_id.is_some() && matches!(method, "ListCrewAnnotations" | "ApplyCrewAnnotation"))
            {
                return Err(denied("Relay authority belongs to another account"));
            }
            if matches!(method, "VerifyImportedCrewSession" | "PublishImportedCrewSession") {
                authority.require_import_controller()?;
            }
            if let Some(peer) = &authority.peer {
                if !matches!(method, "DeliverPeerMessage" | methods::LOCAL_DEVICE)
                    || peer.target_device_id != self.doc_host.device_id()
                    || (method == "DeliverPeerMessage" && params.get("chatId").and_then(Value::as_str) != Some(peer.target_session_id.as_str()))
                {
                    return Err(denied("Peer transport cannot dispatch general engine operations"));
                }
            }
            if let Some(session) = &authority.session_id {
                let bound = if let Some(peer) = &authority.peer {
                    &peer.source_session_id == session
                        && authority.device_id.as_deref() == Some(peer.source_device_id.as_str())
                } else {
                    authority.device_id.as_deref() == Some(self.doc_host.device_id())
                        && params.get("chatId").and_then(Value::as_str).is_none_or(|chat| chat == session)
                };
                if !bound || authority.expires_at.is_none_or(|expires| expires <= crate::now_ms())
                    || authority.grant_id.as_deref().is_none_or(str::is_empty)
                {
                    return Err(denied("Scoped relay authority is expired or belongs to another host/session"));
                }
            }
            if authority.session_id.is_some()
                && params.get("targetDeviceId").and_then(Value::as_str)
                    .is_some_and(|device| device != self.doc_host.device_id())
            {
                return Err(denied("A scoped relay cannot delegate to another device"));
            }
            if super::AuthRpc::handles(method) && method != methods::AUTH_STATUS {
                return Err(denied("Crew authentication changes require local consent"));
            }
        }
        if let Some(crew) = &self.crew
            && !super::AuthRpc::handles(method)
            && !matches!(method, methods::ENGINE_INFO | methods::ENGINE_READY)
        {
            crew.ensure_identity().map_err(failed)?;
        }
        if auth.device_mode() {
            let scope = auth.verified_scope().ok_or_else(|| denied("Sandbox authority unavailable"))?;
            let chat_id = params.get("chatId").and_then(Value::as_str);
            if chat_id.is_some_and(|chat| Some(chat) != scope.session_id.as_deref()) {
                return Err(denied("Sandbox host is bound to a different session"));
            }
            if matches!(method, methods::UPLOAD_CHUNK | methods::UPLOAD_COMMIT)
                && (chat_id.is_none() || !auth.has_capability(zeron_proto::CAPABILITY_SESSION_FILES))
            {
                return Err(denied("Sandbox file access requires its exact session and session.files authority"));
            }
            if matches!(method, methods::LIST_AGENT_ACCOUNTS | methods::ACTIVATE_AGENT_ACCOUNT
                | methods::FORGET_AGENT_ACCOUNT | methods::START_AGENT_LOGIN | methods::COMPLETE_AGENT_LOGIN
                | methods::POLL_AGENT_LOGIN | methods::CANCEL_AGENT_LOGIN | methods::INSTALL_HARNESS)
            {
                return Err(denied("Sandbox hosts cannot manage local credentials or agent installations"));
            }
        }
        Ok(())
    }

    pub(super) async fn crew_rpc(&self, method: &str, mut params: Value) -> Result<RpcReply, RpcError> {
        match method {
            "GetHarnessAutoUpdate" => {
                let coordinator = self.harness_updates.as_ref().ok_or_else(|| denied("Agent update preferences unavailable"))?;
                RpcReply::value(&json!({"enabled": coordinator.harness_auto_update(), "supported": zeron_update::AUTOMATIC_UPDATES_SUPPORTED}))
            }
            "SetHarnessAutoUpdate" => {
                if self.relay { return Err(denied("Update preference changes require local consent")); }
                #[derive(Deserialize)] struct Input { enabled: bool }
                let input: Input = zeron_rpc::parse_params(params)?;
                let coordinator = self.harness_updates.as_ref().ok_or_else(|| denied("Agent update preferences unavailable"))?;
                coordinator.set_harness_auto_update(input.enabled).map_err(failed)?;
                RpcReply::value(&json!({"enabled": coordinator.harness_auto_update(), "supported": zeron_update::AUTOMATIC_UPDATES_SUPPORTED}))
            }
            "ScaffoldHostAuthority" => {
                if self.relay { return Err(denied("Device bootstrap probes require local IPC")); }
                let authority = self.auth()?.device_grant_authority()
                    .ok_or_else(|| denied("Verified sandbox authority unavailable"))?;
                RpcReply::value(&authority)
            }
            "crew.accounts.login.start" | "crew.accounts.login.complete"
            | "crew.accounts.login.poll" | "crew.accounts.login.cancel" => {
                if self.relay { return Err(denied("Shared account login requires local controller consent")); }
                let services = self.crew_services()?;
                services.controller()?;
                match method {
                    "crew.accounts.login.start" => {
                        #[derive(Deserialize)] struct Input { harness: HarnessId }
                        let input: Input = zeron_rpc::parse_params(params)?;
                        RpcReply::value(&self.agent_accounts.start_shared_login(input.harness).await.map_err(failed)?)
                    }
                    "crew.accounts.login.complete" => {
                        #[derive(Deserialize)] #[serde(rename_all = "camelCase")]
                        struct Input { login_id: String, code: String }
                        let input: Input = zeron_rpc::parse_params(params)?;
                        self.agent_accounts.complete_shared_login(&services.runtime().client(), &input.login_id, &input.code).await.map_err(failed)?;
                        RpcReply::value(&json!({}))
                    }
                    "crew.accounts.login.poll" => {
                        #[derive(Deserialize)] #[serde(rename_all = "camelCase")]
                        struct Input { login_id: String }
                        let input: Input = zeron_rpc::parse_params(params)?;
                        RpcReply::value(&self.agent_accounts.poll_shared_login(&services.runtime().client(), &input.login_id).await.map_err(failed)?)
                    }
                    _ => {
                        #[derive(Deserialize)] #[serde(rename_all = "camelCase")]
                        struct Input { login_id: String }
                        let input: Input = zeron_rpc::parse_params(params)?;
                        self.agent_accounts.cancel_shared_login(&input.login_id).await;
                        RpcReply::value(&json!({}))
                    }
                }
            }
            "crew.accounts.importLocal" => {
                if self.relay { return Err(denied("Local credential import requires local consent")); }
                #[derive(Deserialize)] #[serde(rename_all = "camelCase")]
                struct Input { harness: HarnessId, account_id: String }
                let input: Input = zeron_rpc::parse_params(params)?;
                let value = self.crew_services()?.import_local_account(&self.agent_accounts, input.harness, &input.account_id).await?;
                RpcReply::value(&value)
            }
            "ResolveCrewSessionLink" => self.resolve_crew_link(params).await,
            "ListCrewAnnotations" | "ApplyCrewAnnotation" => self.crew_annotations(method, params).await,
            "SearchSessions" => {
                if let Some(params) = params.as_object_mut() { params.remove("__crewAuthority"); params.remove("targetDeviceId"); }
                if self.auth()?.device_mode() { return Err(denied("Session search requires a controller")); }
                let owner = self.auth()?.verified_owner_email().ok_or_else(|| denied("Verified directory owner unavailable"))?;
                own_directory_search(&mut params, &owner)?;
                let value = self.crew_services()?.runtime().client().search_crew_sessions(&params).await.map_err(failed)?;
                RpcReply::value(&value)
            }
            _ => {
                if let Some(params) = params.as_object_mut() { params.remove("__crewAuthority"); params.remove("targetDeviceId"); }
                let control = params.get("control").cloned()
                    .map(serde_json::from_value::<ScaffoldEnvironmentControl>).transpose().map_err(failed)?;
                if let Some(ScaffoldEnvironmentControl::HandoffOmpSession { scope, native_session_id, .. }) = &control {
                    if native_session_id.contains('/') || native_session_id.contains('\\') {
                        return Err(denied("Public handoff control requires a native identity, not an artifact path; use session handoff for a verified fork"));
                    }
                    let session_id = scope.session_id.as_deref().ok_or_else(|| denied("Native handoff requires its exact target session"))?;
                    self.sessions.ensure_execution_allowed(session_id, Some(native_session_id)).map_err(failed)?;
                }
                if method == "scaffold.control"
                    && let Some(ScaffoldEnvironmentControl::Start { sandbox_id, scope, expected_lifecycle_epoch }) = &control
                {
                    if !self.workspace.registry_synced() {
                        return Err(denied("Start requires synchronized session history before changing the sandbox"));
                    }
                    let session_id = scope.session_id.as_deref().ok_or_else(|| denied("Start requires an exact session"))?;
                    let expected = self.crew_services()?.session_metadata(session_id).map_err(failed)?;
                    if scope != &expected.scope { return Err(denied("Start belongs to another project or deployment")); }
                    let chat = self.workspace.chat(session_id).map_err(failed)?;
                    let metadata = chat.as_ref().and_then(|chat| chat.crew.as_ref());
                    if metadata.is_some_and(|metadata| metadata.owner_subject != expected.owner_subject || metadata.scope != expected.scope) {
                        return Err(denied("Start session belongs to another owner or deployment"));
                    }
                    validate_start_history(sandbox_id, *expected_lifecycle_epoch,
                        self.sessions.original_epoch_history(session_id), metadata.and_then(|metadata| metadata.original_epoch_history.as_ref()))?;
                }
                let route = control.and_then(|control| match control {
                        ScaffoldEnvironmentControl::Create { agent_route, .. }
                        | ScaffoldEnvironmentControl::UpdateAgentRoute { agent_route, .. } => Some(agent_route),
                        _ => None,
                    });
                let mut value = self.crew_services()?.dispatch(method, params).await?;
                if method == "crew.status" { value["managed"] = json!(self.registry.managed_inference()); }
                if matches!(method, "scaffold.control" | "scaffold.reconcile" | "scaffold.outcome")
                    && value.get("status").and_then(Value::as_str) == Some("confirmed")
                    && let Some(result) = value.get("result").filter(|result| !result.is_null())
                {
                    let result: ScaffoldEnvironmentControlResult = serde_json::from_value(result.clone()).map_err(failed)?;
                    let command = value.get("commandId").and_then(Value::as_str).map(str::to_owned);
                    let chat = self.publish_crew_environment_with_start(result, route, command).await?;
                    // Platform uses verified email; every client-facing Crew owner is a subject.
                    value["result"]["environment"]["ownerPrincipal"] =
                        json!(chat.crew.ok_or_else(|| denied("Published Crew identity unavailable"))?.owner_subject);
                } else if method == "scaffold.refresh" {
                    let mut snapshot: ScaffoldEnvironmentSnapshot = serde_json::from_value(value.clone()).map_err(failed)?;
                    for environment in &mut snapshot.environments {
                        // Listing discovers environments, never starts or attaches them.
                        let chat = self.publish_crew_environment(ScaffoldEnvironmentControlResult {
                            environment: environment.clone(), preparation_generation: None, attached_device_id: None, run_id: None,
                            room_projection: None, control_grant: None, handoff_native_session_id: None, handoff_cwd: None,
                        }, None).await?;
                        environment.owner_principal = chat.crew
                            .ok_or_else(|| denied("Published Crew identity unavailable"))?.owner_subject;
                    }
                    value = serde_json::to_value(snapshot).map_err(failed)?;
                }
                RpcReply::value(&value)
            }
        }
    }

    pub(crate) async fn reconcile_crew_registry(&self) {
        let mut registry = self.workspace.watch_registry();
        loop {
            while !self.workspace.registry_synced() {
                if registry.changed().await.is_err() { return; }
            }
            let refresh = async {
                let services = self.crew_services()?;
                let mut after = String::new();
                while let Some(results) = services.reconcile_pending_batch(&mut after).await? {
                    for result in results { self.publish_crew_environment(result, None).await?; }
                }
                let scope = self.auth()?.verified_scope().ok_or_else(|| denied("Crew sign-in required"))?;
                Box::pin(self.crew_rpc("scaffold.refresh", serde_json::to_value(scope).map_err(failed)?)).await?;
                Ok::<(), RpcError>(())
            };
            if let Err(error) = refresh.await {
                tracing::warn!(%error, "Crew environment observation retained for reconnect");
            }
            // Observations only. No automatic create, resume, attach or prompt admission.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    }

    pub(super) async fn publish_crew_environment(&self, result: ScaffoldEnvironmentControlResult,
        route: Option<zeron_proto::AgentRoute>) -> Result<Chat, RpcError> {
        self.publish_crew_environment_with_start(result, route, None).await
    }

    async fn publish_crew_environment_with_start(&self, result: ScaffoldEnvironmentControlResult,
        route: Option<zeron_proto::AgentRoute>, start_command: Option<String>) -> Result<Chat, RpcError> {
        if !self.workspace.registry_synced() { return Err(denied("Sandbox outcome is durable; its session reference awaits registry synchronization")); }
        let services = self.crew_services()?.clone();
        let auth = self.auth()?.clone();
        let workspace = self.workspace.clone();
        let sessions = self.sessions.clone();
        let store = self.doc_host.crew_store();
        let start_gate = match start_command.as_deref() {
            Some(command) => services.start_publication(command, &result.environment).await?,
            None => None,
        };
        tokio::task::spawn_blocking(move || {
            let explicit_start = start_gate.is_some();
            let _start_gate = start_gate;
            let session_id = result.environment.scope.session_id.as_deref()
                .ok_or_else(|| denied("Environment has no stable session identity"))?;
            let mut metadata = services.session_metadata(session_id).map_err(failed)?;
            if result.environment.scope != metadata.scope
                || !auth.verified_owner_email().is_some_and(|email| email.eq_ignore_ascii_case(&result.environment.owner_principal))
            { return Err(denied("Environment owner or scope differs from this profile")); }
            let SessionEnvironmentSource::Scaffold { sandbox_id, lifecycle_epoch: Some(epoch), lifecycle, .. } = &result.environment.source else {
                return Err(denied("Scaffold environment lacks its authoritative lifecycle binding"));
            };
            let host = crate::scaffold::device_id_for(sandbox_id, *epoch);
            if result.attached_device_id.as_ref().is_some_and(|device| device != &host) {
                return Err(denied("Attached host differs from the verified lifecycle binding"));
            }
            if let Some(projection) = &result.room_projection
                && (projection.project_id != metadata.scope.project_id
                    || Some(&projection.deployment_id) != metadata.scope.deployment_id.as_ref()
                    || projection.session_id != session_id)
            { return Err(denied("Attached room differs from the verified session")); }
            let existing = workspace.chat(session_id).map_err(failed)?;
            let trusted_history = sessions.original_epoch_history(session_id).cloned();
            let mut release = None;
            let mut reset_native = false;
            if explicit_start {
                let binding = json!({"sandboxId":sandbox_id,"lifecycleEpoch":epoch,"deviceId":host});
                let (key, request) = if let Some(history) = &trusted_history {
                    if history.sandbox_id != *sandbox_id || *epoch <= history.coordinator_lifecycle_epoch {
                        return Err(denied("Start differs from the immutable original epoch"));
                    }
                    (format!("original-epoch-start:{session_id}:{}:{epoch}", history.lifecycle_epoch), serde_json::to_vec(history).map_err(failed)?)
                } else {
                    if existing.as_ref().and_then(|chat| chat.crew.as_ref()).and_then(|metadata| metadata.original_epoch_history.as_ref()).is_some() {
                        return Err(denied("Original epoch receipt is not trusted by this profile"));
                    }
                    (format!("start-association:{session_id}:{epoch}"), serde_json::to_vec(&json!({"scope":result.environment.scope,"binding":binding})).map_err(failed)?)
                };
                store.claim_crew_command(&key, &request).map_err(failed)?;
                let prior = store.crew_command(&key).map_err(failed)?.and_then(|(_, outcome)| outcome);
                if let Some(prior) = prior {
                    if serde_json::from_slice::<Value>(&prior).map_err(failed)? != binding {
                        return Err(denied("New epoch association conflicts with its immutable Start receipt"));
                    }
                } else {
                    reset_native = true;
                    release = Some((key, serde_json::to_vec(&binding).map_err(failed)?));
                }
            }
            if let Some(old) = existing.as_ref().and_then(|chat| chat.crew.as_ref()) {
                if old.owner_subject != metadata.owner_subject || old.scope != metadata.scope {
                    return Err(denied("Session reference belongs to another owner or deployment"));
                }
                if let Some(previous) = &old.environment
                    && let SessionEnvironmentSource::Scaffold { sandbox_id: previous_id, lifecycle_epoch: previous_epoch, lifecycle: previous_lifecycle, .. } = &previous.source
                {
                    if previous_id != sandbox_id { return Err(denied("Session is already bound to another sandbox")); }
                    if previous_epoch.is_some_and(|old_epoch| old_epoch > *epoch)
                        || (matches!(previous_lifecycle, ScaffoldLifecycle::Stopped | ScaffoldLifecycle::Failed)
                            && !matches!(lifecycle, ScaffoldLifecycle::Stopped | ScaffoldLifecycle::Failed) && !explicit_start)
                    { return Ok(existing.expect("existing metadata")); }
                }
                metadata.agent_route = old.agent_route.clone();
                metadata.original_epoch_history = trusted_history.clone().or_else(|| old.original_epoch_history.clone());
            }
            let observed: Option<zeron_proto::AgentRoute> = result.environment.unknown.get("agentRoute")
                .map(|value| serde_json::from_value(value.clone()).map_err(failed)).transpose()?;
            let update_config = existing.as_ref().is_none_or(|chat| chat.config.is_none())
                || (!explicit_start && (route.is_some()
                    || observed.as_ref().is_some_and(|route| Some(route) != metadata.agent_route.as_ref())));
            metadata.agent_route = route.or(observed).or(metadata.agent_route);
            let mut environment = result.environment.clone();
            // Platform's sandbox owner is a verified email; the index uses the verified subject.
            environment.owner_principal = metadata.owner_subject.clone();
            metadata.environment = Some(environment);
            metadata.validate().map_err(denied)?;
            let device_missing = !workspace.read_devices().map_err(failed)?.iter().any(|device| device.id == host);
            if !reset_native && !device_missing && !update_config && existing.as_ref().is_some_and(|chat| {
                chat.device_id == host && chat.room_gen == Some(2) && chat.cwd.is_some() && chat.crew.as_ref() == Some(&metadata)
                    && result.handoff_cwd.as_ref().is_none_or(|cwd| chat.cwd.as_ref() == Some(cwd) && chat.harness_session_cwd.as_ref() == Some(cwd))
                    && result.handoff_native_session_id.as_ref().is_none_or(|native| chat.harness_session_id.as_ref() == Some(native))
            }) { return Ok(existing.expect("unchanged session")); }
            let mut chat = existing.unwrap_or_else(|| Chat {
                id: session_id.to_owned(), device_id: host.clone(), title: result.environment.name.clone(),
                archived: false, cwd: None, branch: None, checkout_id: None, source_context: None,
                config: None, last_message_preview: None, last_message_at: None,
                created_at: chrono::Utc::now(), harness_session_id: None, harness_session_cwd: None,
                space_id: None, last_seen_at: None, room_gen: Some(2), parent_chat_id: None, crew: None,
            });
            bind_scaffold_workspace(&mut chat, &host, sandbox_id, result.handoff_cwd);
            chat.room_gen = Some(2);
            if update_config && let Some(route) = &metadata.agent_route {
                let config = chat.config.get_or_insert_with(|| ChatConfig {
                    harness: HarnessId::Omp, model: None, reasoning: None,
                    model_options: Default::default(), sandbox: zeron_proto::SandboxLevel::DangerFullAccess,
                });
                config.harness = HarnessId::Omp;
                config.model = Some(route.omp_model());
                if let Some(account) = &route.account_id { config.model_options.insert("crewAccountId".into(), json!(account)); }
                else { config.model_options.remove("crewAccountId"); }
            }
            if let Some(native) = result.handoff_native_session_id { chat.harness_session_id = Some(native); }
            if reset_native {
                // Empty ID is the durable do-not-resume tombstone; None would fall back to the old journal.
                chat.harness_session_id = Some(String::new());
                chat.harness_session_cwd = None;
            }
            chat.crew = Some(metadata);
            let device = device_missing.then(|| Device {
                id: host, name: result.environment.name.unwrap_or_else(|| "Crew sandbox".into()),
                platform: "linux".into(), last_seen_at: None, created_at: None, version: None,
                devbox_provider: None, devbox_id: None,
                cursor_sdk_version: None, capabilities: Vec::new(),
            });
            workspace.publish_crew_chat(&chat, device.as_ref()).map_err(failed)?;
            if let Some((key, outcome)) = release {
                sessions.reset_original_epoch_native_association(session_id);
                store.finish_crew_command(&key, &outcome).map_err(failed)?;
            }
            Ok(chat)
        }).await.map_err(failed)?
    }

    async fn resolve_crew_link(&self, params: Value) -> Result<RpcReply, RpcError> {
        if self.auth()?.device_mode() { return Err(denied("Session links require a signed-in controller")); }
        #[derive(Deserialize)] struct Input { url: String }
        let input: Input = zeron_rpc::parse_params(params)?;
        let services = self.crew_services()?;
        let scope = self.auth()?.verified_scope().ok_or_else(|| denied("Verified scope unavailable"))?;
        let schemes = ["crew-upstream-canary", "comet", "comet-staging", "crew", "crew-staging"];
        let directory = schemes.iter().find_map(|scheme| CrewSessionLink::parse_deep_link(&input.url, scheme));
        let sandbox = schemes.iter().find_map(|scheme| ScaffoldSessionLink::parse_deep_link(&input.url, scheme));
        let (project, deployment, session_id) = if let Some(link) = &directory {
            (&link.project_id, link.deployment_id.as_deref().or(scope.deployment_id.as_deref()), link.session_id.as_str())
        } else if let Some(link) = &sandbox {
            (&link.scope.project_id, link.scope.deployment_id.as_deref(), link.scope.session_id.as_deref().unwrap_or_default())
        } else { return Err(denied("Invalid Crew session link")); };
        if project != &scope.project_id || deployment != scope.deployment_id.as_deref() {
            return Err(denied("Session link belongs to another project or deployment"));
        }
        uuid::Uuid::parse_str(session_id).map_err(|_| denied("Invalid Crew session identity"))?;
        let mut changes = self.workspace.watch_registry();
        let row = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let Some(chat) = self.workspace.chat(session_id).map_err(failed)? { return Ok(chat); }
                if self.workspace.registry_synced() {
                    let mut lookup = scope.clone();
                    lookup.session_id = Some(session_id.to_owned());
                    let client = services.runtime().client();
                    let cancellation = tokio_util::sync::CancellationToken::new();
                    let environment = if let Some(link) = &sandbox {
                        client.inspect(&link.sandbox_id, &lookup, &cancellation).await.map_err(failed)?
                    } else {
                        client.list(&lookup, &cancellation).await.map_err(failed)?.into_iter().next()
                            .ok_or_else(|| denied("Session is not in this isolated Crew registry; legacy sessions require explicit import"))?
                    };
                    // A cold client may not have indexed this existing sandbox yet.
                    // Observe its identity only; no execution or attachment is admitted.
                    return self.publish_crew_environment(ScaffoldEnvironmentControlResult {
                        environment, preparation_generation: None, attached_device_id: None, run_id: None,
                        room_projection: None, control_grant: None, handoff_native_session_id: None, handoff_cwd: None,
                    }, None).await;
                }
                changes.changed().await.map_err(failed)?;
            }
        }).await.map_err(|_| denied("Session lookup is pending; retry after reconnect"))??;
        let expected = services.session_metadata(session_id).map_err(failed)?;
        let metadata = row.crew.as_ref().ok_or_else(|| denied("Session identity has not been verified by its host"))?;
        if metadata.owner_subject != expected.owner_subject || metadata.scope != expected.scope {
            return Err(denied("Session link is not authorized in this profile"));
        }
        if let Some(link) = sandbox {
            if !metadata.environment.as_ref().is_some_and(|environment| matches!(&environment.source,
                SessionEnvironmentSource::Scaffold { sandbox_id, .. } if sandbox_id == &link.sandbox_id))
            { return Err(denied("Session link names another sandbox")); }
        }
        // Navigation changes focus only; no attach, resume, transfer, or command admission.
        RpcReply::value(&json!({"chatId": row.id}))
    }

    fn annotation_principal(&self, params: &Value, metadata: &zeron_proto::CrewChatMetadata)
        -> Result<(CollaborationPrincipal, Option<CapabilityGrant>), RpcError> {
        let authority = if self.relay {
            serde_json::from_value::<RelayAuthority>(params.get("__crewAuthority").cloned()
                .ok_or_else(|| denied("Missing verified annotation caller"))?).map_err(failed)?
        } else {
            let session = self.auth()?.verified_scaffold_session().ok_or_else(|| denied("Crew sign-in required"))?;
            let device = session.device;
            RelayAuthority {
                subject: session.identity.user_id, project_id: session.identity.project_scope,
                deployment_id: session.identity.deployment_id,
                session_id: device.as_ref().map(|device| device.session_id.clone()),
                device_id: device.as_ref().map(|device| device.device_id.clone()),
                grant_id: device.as_ref().map(|device| device.grant_id.clone()),
                expires_at: device.as_ref().map(|device| device.expires_at),
                lifecycle_epoch: device.as_ref().map(|device| device.lifecycle_epoch),
                capabilities: session.identity.capabilities,
                peer: None,
            }
        };
        let principal = CollaborationPrincipal {
            subject: authority.subject, email: None, project_id: authority.project_id,
            deployment_id: authority.deployment_id, session_id: authority.session_id,
            capabilities: authority.capabilities, unknown: Default::default(),
        };
        let grant = if principal.session_id.is_some() {
            let expires = authority.expires_at.filter(|expiry| *expiry > crate::now_ms())
                .ok_or_else(|| denied("Annotation capability expired"))?;
            Some(CapabilityGrant {
                id: authority.grant_id.ok_or_else(|| denied("Annotation capability missing"))?,
                principal_subject: principal.subject.clone(), scope: metadata.scope.clone(),
                capabilities: principal.capabilities.clone(), sandbox_id: None,
                device_id: authority.device_id, lifecycle_epoch: authority.lifecycle_epoch,
                granted_by: metadata.owner_subject.clone(), granted_at: crate::now_ms(),
                expires_at: Some(expires), revoked_at: None, unknown: Default::default(),
            })
        } else { None };
        Ok((principal, grant))
    }

    async fn crew_annotations(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
        let chat_id = params.get("chatId").and_then(Value::as_str).ok_or_else(|| denied("Missing chatId"))?;
        let chat = self.crew_services()?.adopt_chat(&self.workspace, chat_id).map_err(failed)?;
        let metadata = chat.crew.clone().ok_or_else(|| denied("Missing verified session metadata"))?;
        let (principal, grant) = self.annotation_principal(&params, &metadata)?;
        let capability = if method == "ApplyCrewAnnotation" { CAPABILITY_SESSION_ANNOTATE } else { CAPABILITY_SESSION_READ };
        if !metadata.permits(&principal, grant.as_ref(), capability, crate::now_ms()) {
            return Err(denied("Annotation access denied"));
        }
        if method == "ApplyCrewAnnotation" && chat.device_id != self.doc_host.device_id() {
            if principal.session_id.is_some() { return Err(denied("Scoped annotations must be sent directly to the execution host")); }
            return self.forward(&chat.device_id, method, params).await;
        }
        let host = self.doc_host.clone();
        let id = chat_id.to_owned();
        if method == "ApplyCrewAnnotation" {
            #[derive(Deserialize)] #[serde(rename_all = "camelCase")]
            struct Input { command_id: String, command: AnnotationCommand, scope: CollaborationScope }
            let input: Input = zeron_rpc::parse_params(params)?;
            if input.scope != metadata.scope || input.command_id.is_empty() || input.command_id.len() > 256 {
                return Err(denied("Invalid annotation scope or immutable command ID"));
            }
            let annotation = tokio::task::spawn_blocking(move || {
                let store = host.crew_store();
                let ledger_id = format!("annotation:{}:{}", id, input.command_id);
                let immutable = serde_json::to_vec(&json!({"principal":principal.subject,"scope":metadata.scope,"command":input.command})).map_err(failed)?;
                let fresh = store.claim_crew_command(&ledger_id, &immutable).map_err(failed)?;
                if let Some((_, Some(result))) = store.crew_command(&ledger_id).map_err(failed)? {
                    return serde_json::from_slice::<SemanticAnnotation>(&result).map_err(failed);
                }
                let handle = host.open(&id).map_err(failed)?;
                let annotation = if fresh {
                    handle.apply_crew_annotation(&input.command_id, &input.command, &principal, grant.as_ref(), &metadata).map_err(failed)?
                } else {
                    handle.doc().annotation_command_result(&input.command_id).map_err(failed)?
                        .ok_or_else(|| denied("Annotation outcome unknown; reconcile the same command after synchronization, never replay"))?
                };
                host.persist_fork(&handle).map_err(failed)?;
                store.finish_crew_command(&ledger_id, &serde_json::to_vec(&annotation).map_err(failed)?).map_err(failed)?;
                Ok(annotation)
            }).await.map_err(failed)??;
            return RpcReply::value(&annotation);
        }
        let (annotations, entries) = tokio::task::spawn_blocking(move || {
            let handle = host.open(&id).map_err(failed)?;
            let annotations = handle.doc().annotations().map_err(failed)?;
            let entries = if annotations.iter().any(|annotation| annotation.anchor.target_kind != AnchorTargetKind::File) {
                handle.doc().read_entries().map_err(failed)?
            } else { Vec::new() };
            Ok::<_, RpcError>((annotations, entries))
        }).await.map_err(failed)??;
        let messages: std::collections::HashMap<_, _> = entries.iter().map(|entry| (entry.id.as_str(), entry)).collect();
        let tools: std::collections::HashMap<_, _> = entries.iter().flat_map(|entry| &entry.parts)
            .filter_map(|part| match part { zeron_doc::MessagePart::Tool { id, .. } => Some((id.as_str(), part)), _ => None }).collect();
        let mut blobs = std::collections::HashMap::new();
        let mut projected = Vec::with_capacity(annotations.len());
        for annotation in annotations {
            let text: Option<std::borrow::Cow<'_, str>> = match annotation.anchor.target_kind {
                AnchorTargetKind::Message => messages.get(annotation.anchor.target_id.as_str())
                    .map(|entry| std::borrow::Cow::Owned(entry.parts.iter().filter_map(|part| match part {
                        zeron_doc::MessagePart::Text { text, .. } => Some(text.as_str()), _ => None,
                    }).enumerate().fold(String::new(), |mut text, (index, part)| {
                        if index > 0 { text.push('\n'); }
                        text.push_str(part);
                        text
                    }))),
                AnchorTargetKind::ToolCall => {
                    if let Some(zeron_doc::MessagePart::Tool { call, output, output_ref, .. }) = tools.get(annotation.anchor.target_id.as_str()) {
                        if let Some(reference) = output_ref.as_deref().filter(|reference| reference.split_once('/').is_some_and(|(chat, _)| chat == chat_id)) {
                            if !blobs.contains_key(reference) {
                                blobs.insert(reference, self.doc_host.fetch_tool_blob(reference).await.ok());
                            }
                            blobs.get(reference).and_then(Option::as_deref).map(std::borrow::Cow::Borrowed)
                        } else {
                            output.as_deref().map(std::borrow::Cow::Borrowed)
                                .or_else(|| serde_json::to_string(call).ok().map(std::borrow::Cow::Owned))
                        }
                    } else { None }
                }
                AnchorTargetKind::File => {
                    if let Some(FileTargetReference::LocalWorkspacePath { workspace_id, relative_path, .. }) = &annotation.anchor.file
                        && chat.device_id == self.doc_host.device_id()
                        && chat.space_id.as_deref() == Some(workspace_id)
                        && !std::path::Path::new(relative_path).is_absolute()
                    {
                        self.workspace_files.read_file(zeron_proto::ReadWorkspaceFileRequest {
                            target: zeron_proto::WorkspaceTarget { chat_id: None, space_id: Some(workspace_id.clone()), checkout_path: None },
                            path: relative_path.clone(),
                        }).await.ok().filter(|file| !file.truncated).and_then(|file| file.text).map(std::borrow::Cow::Owned)
                    } else { None }
                }
            };
            projected.push(zeron_doc::reanchor_annotation(&annotation, text.as_deref()));
        }
        RpcReply::value(&projected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_publication_rebinds_local_workspace_and_preserves_remote_worktrees() {
        let host = crate::scaffold::device_id_for("box", 2);
        let previous_host = crate::scaffold::device_id_for("box", 1);
        for (name, device, remote, cwd, handoff, expected) in [
            ("local project", "controller", false, Some("/Users/czhen"), None, "/workspace/ashler-platform"),
            ("local project fallback", "controller", false, None, None, "/workspace/ashler-platform"),
            ("local handoff", "controller", false, Some("/Users/czhen"), Some("/workspace/crew-handoff"), "/workspace/crew-handoff"),
            ("same host", host.as_str(), true, Some("/workspace/worktree"), None, "/workspace/worktree"),
            ("new epoch", previous_host.as_str(), true, Some("/workspace/worktree"), None, "/workspace/worktree"),
            ("verified handoff", host.as_str(), true, Some("/workspace/worktree"), Some("/workspace/crew-handoff"), "/workspace/crew-handoff"),
        ] {
            let mut chat: Chat = serde_json::from_value(json!({
                "id": "session", "deviceId": device, "archived": false,
                "createdAt": "2026-10-26T00:00:00Z", "cwd": cwd, "spaceId": "project",
                "config": {"harness": "omp", "model": "openai-codex/gpt-6-astra",
                    "modelOptions": {}, "sandbox": "workspace-write"},
                "harnessSessionCwd": if remote { cwd } else { None },
                "crew": if remote { json!({
                    "ownerSubject": "owner", "sessionId": "session",
                    "scope": {"projectId": "project", "deploymentId": "deployment", "sessionId": "session"},
                    "environment": {
                        "source": {"kind": "scaffold", "sandbox_id": "box", "lifecycle": "ready", "lifecycle_epoch": 1},
                        "ownerPrincipal": "owner",
                        "scope": {"projectId": "project", "deploymentId": "deployment", "sessionId": "session"}
                    }
                }) } else { Value::Null }
            })).unwrap();
            let config = chat.config.clone();
            bind_scaffold_workspace(&mut chat, &host, "box", handoff.map(str::to_owned));
            assert_eq!(chat.device_id, host, "{name}");
            assert_eq!(chat.cwd.as_deref(), Some(expected), "{name}");
            assert_eq!(chat.space_id.as_deref(), if remote { Some("project") } else { None }, "{name}");
            assert_eq!(chat.harness_session_cwd.as_deref(), handoff.or(if remote { cwd } else { None }), "{name}");
            assert_eq!(chat.config, config, "{name}");
        }
    }

    #[test]
    fn start_preflight_rejects_untrusted_history_before_control_dispatch() {
        let history = zeron_proto::OriginalEpochHistory {
            sandbox_id: "box".into(), lifecycle_epoch: 4, coordinator_lifecycle_epoch: 7,
            source_device_id: "original-host".into(), original_status: "stopped".into(),
            native_session_id: Some("original-native".into()), source_evidence_sha256: "a".repeat(64),
        };
        assert!(validate_start_history("box", 7, None, Some(&history)).is_err());
        assert!(validate_start_history("box", 7, None, None).is_ok(), "fresh sessions remain startable");
        assert!(validate_start_history("box", 7, Some(&history), Some(&history)).is_ok());
        assert!(validate_start_history("box", 7, Some(&history), None).is_ok(), "a missing synced marker cannot erase trusted history");
        assert!(validate_start_history("other", 7, Some(&history), None).is_err());
        assert!(validate_start_history("box", 6, Some(&history), None).is_err());
        let mut altered = history.clone();
        altered.source_evidence_sha256 = "b".repeat(64);
        assert!(validate_start_history("box", 7, Some(&history), Some(&altered)).is_err());
    }

    #[test]
    fn imported_profile_proof_never_accepts_session_or_peer_grants() {
        let owner = json!({"subject": "owner", "projectId": "project",
            "deploymentId": "deployment", "capabilities": ["session.read"]});
        let authority: RelayAuthority = serde_json::from_value(owner.clone()).unwrap();
        assert!(authority.require_import_controller().is_ok());
        for (key, value) in [("sessionId", json!("session")), ("grantId", json!("grant")),
            ("peer", json!({"sourceSessionId": "source", "sourceDeviceId": "source-device",
                "targetSessionId": "target", "targetDeviceId": "target-device"}))] {
            let mut scoped = owner.clone();
            scoped[key] = value;
            let authority: RelayAuthority = serde_json::from_value(scoped).unwrap();
            assert!(authority.require_import_controller().is_err());
        }
        assert!(!scaffold_host_rpc_allowed("VerifyImportedCrewSession",
            &json!({"logicalSessionId": "session", "chatId": "session"}), "session"));
    }

    #[test]
    fn personal_directory_search_accepts_cli_defaults_and_preserves_owner_scope() {
        let cli = json!({"query": "deprecat", "sourceUrl": null, "ownerId": null, "limit": 50, "cursor": null});
        let mut request = cli.clone();
        own_directory_search(&mut request, "owner@example.com").unwrap();
        assert_eq!(request, json!({"query": "deprecat", "ownerId": "owner@example.com", "limit": 50}));

        for owner in [None, Some(json!(null)), Some(json!("owner@example.com"))] {
            let mut request = json!({"query": null, "sourceUrl": "https://example.slack.com/archives/C123/p1234567890123456",
                "limit": null, "cursor": "next-page"});
            if let Some(owner) = owner { request["ownerId"] = owner; }
            own_directory_search(&mut request, "owner@example.com").unwrap();
            assert_eq!(request, json!({"sourceUrl": "https://example.slack.com/archives/C123/p1234567890123456",
                "ownerId": "owner@example.com", "cursor": "next-page"}));
        }

        for owner in [json!("another@example.com"), json!(""), json!(false), json!(42), json!([]), json!({})] {
            let mut request = cli.clone();
            request["ownerId"] = owner;
            assert!(own_directory_search(&mut request, "owner@example.com").is_err());
        }
        let mut invalid = json!(null);
        assert!(own_directory_search(&mut invalid, "owner@example.com").is_err());
    }

    #[test]
    fn scoped_host_admission_keeps_device_watches_off_session_links() {
        let device = json!({"targetDeviceId": "host"});
        let session = json!({"targetDeviceId": "host", "chatId": "session"});
        for method in [methods::WATCH_HARNESS_UPDATES, methods::WATCH_CHECKOUT_DIFFS, methods::LIST_MODELS] {
            assert!(!scaffold_host_rpc_allowed(method, &device, "session"));
            assert!(!scaffold_host_rpc_allowed(method, &session, "session"));
        }
        assert!(scaffold_host_rpc_allowed(methods::WATCH_DOC_MESSAGES, &session, "session"));
        assert!(!scaffold_host_rpc_allowed(methods::WATCH_DOC_MESSAGES, &session, "other"));
        assert!(scaffold_host_rpc_allowed(methods::QUEUE_COMMAND,
            &json!({"chatId": "session", "command": {"kind": "run"}}), "session"));
        assert!(!scaffold_host_rpc_allowed(methods::QUEUE_COMMAND,
            &json!({"chatId": "session", "command": {"kind": "unknown"}}), "session"));
    }
}
