//! Crew control-plane integration on the upstream command ledger, not a shared CRDT.
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use zeron_harness::{CancellationToken, InferenceRoute};
use zeron_proto::{AgentRoute, CollaborationScope, HarnessId, ScaffoldDatabaseEnvironment,
    ScaffoldEnvironmentControl as Control, ScaffoldEnvironmentControlResult as ControlResult,
    ScaffoldLifecycle, SessionEnvironment, SessionEnvironmentSource};
use zeron_rpc::RpcError;
use zeron_sync::DocsStore;
use crate::{EngineError, auth::Auth, inference_relay::InferenceRelay,
    scaffold::{ScaffoldError, ScaffoldRuntime}};

#[derive(Clone)]
pub struct CrewServices {
    auth: Auth,
    profile_key: String,
    runtime: ScaffoldRuntime,
    store: Arc<DocsStore>,
    relay: InferenceRelay,
    // ponytail: serialize control operations per controller; per-sandbox locks if needed.
    control_gate: Arc<Mutex<()>>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Command {
    principal_id: String,
    control: Control,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlOutcome {
    pub command_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ControlResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffRequest {
    pub command_id: String,
    pub source_session_id: String,
    pub native_session_id: String,
    pub cwd: String,
    pub prompt: String,
    pub agent_route: AgentRoute,
    pub database_environment: ScaffoldDatabaseEnvironment,
    pub recover_sandbox_id: Option<String>,
    pub recover_chat_id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffReceipt {
    pub chat_id: String,
    pub sandbox_id: String,
    pub command_id: String,
    pub environment: SessionEnvironment,
    pub attachment: ControlResult,
    pub native_session_id: String,
    pub cwd: String,
}

impl CrewServices {
    pub fn new(auth: Auth, runtime: ScaffoldRuntime, store: Arc<DocsStore>) -> Result<Self, EngineError> {
        let profile_key = auth.verified_profile_key()
            .ok_or_else(|| EngineError::Other("Crew services require a verified profile".into()))?;
        let relay = InferenceRelay::start(runtime.client())?;
        Ok(Self { auth, profile_key, runtime, store, relay, control_gate: Arc::new(Mutex::new(())) })
    }

    pub fn runtime(&self) -> &ScaffoldRuntime { &self.runtime }

    pub fn session_metadata(&self, session_id: &str) -> Result<zeron_proto::CrewChatMetadata, EngineError> {
        let (owner_subject, mut scope) = self.identity().map_err(|error| EngineError::Other(error.to_string()))?;
        if scope.session_id.as_deref().is_some_and(|bound| bound != session_id) {
            return Err(EngineError::Other("Session differs from the verified device binding".into()));
        }
        scope.session_id = Some(session_id.to_owned());
        Ok(zeron_proto::CrewChatMetadata {
            owner_subject, scope, session_id: session_id.to_owned(), environment: None, agent_route: None,
        })
    }

    pub fn adopt_chat(&self, workspace: &crate::WorkspaceHost, chat_id: &str) -> Result<zeron_proto::Chat, EngineError> {
        let mut chat = workspace.chat(chat_id)?
            .ok_or_else(|| EngineError::Other("Crew session is not present in this profile".into()))?;
        let expected = self.session_metadata(chat_id)?;
        if let Some(metadata) = &chat.crew {
            metadata.validate().map_err(|error| EngineError::Other(error.into()))?;
            if metadata.owner_subject != expected.owner_subject || metadata.scope != expected.scope {
                return Err(EngineError::Other("Crew session belongs to another owner or deployment".into()));
            }
        } else {
            chat.crew = Some(expected);
            workspace.import_chat_row(&chat)?;
        }
        Ok(chat)
    }

    pub fn ensure_identity(&self) -> Result<(), EngineError> {
        self.identity().map(|_| ()).map_err(|error| EngineError::Other(error.to_string()))
    }

    pub async fn generate_title(&self, chat_id: &str, prompt: &str) -> Result<String, EngineError> {
        let (_, scope) = self.identity().map_err(|error| EngineError::Other(error.to_string()))?;
        if self.auth.device_mode() && scope.session_id.as_deref() != Some(chat_id) {
            return Err(EngineError::Other("Sandbox title request belongs to another session".into()));
        }
        self.runtime.client().generate_crew_title(chat_id, Some(self.runtime.deployment_id()), prompt)
            .await.map_err(|error| EngineError::Other(error.to_string()))
    }

    fn identity(&self) -> Result<(String, CollaborationScope), RpcError> {
        if self.auth.verified_profile_key().as_deref() != Some(self.profile_key.as_str()) {
            return Err(denied("Crew profile changed; reopen its isolated store"));
        }
        let principal = self.auth.user_id().ok_or_else(|| denied("Crew sign-in required"))?;
        let scope = self.auth.verified_scope().ok_or_else(|| denied("Verified Crew scope required"))?;
        Ok((principal, scope))
    }

    pub(crate) fn controller(&self) -> Result<(String, CollaborationScope), RpcError> {
        let identity = self.identity()?;
        if self.auth.device_mode() { return Err(denied("Sandbox hosts cannot control other environments or shared accounts")); }
        Ok(identity)
    }

    fn validate_scope(&self, scope: &CollaborationScope) -> Result<String, RpcError> {
        let (principal, verified) = self.controller()?;
        if scope.project_id != verified.project_id
            || scope.deployment_id.as_deref() != Some(self.runtime.deployment_id())
            || scope.deployment_id != verified.deployment_id
            || scope.session_id.as_deref().is_none_or(|id| id.trim().is_empty())
        { return Err(denied("Wrong Crew project, deployment, or session")); }
        Ok(principal)
    }

    fn validate_environment(&self, environment: &SessionEnvironment, scope: &CollaborationScope) -> Result<(), RpcError> {
        self.validate_scope(scope)?;
        let owner = self.auth.verified_owner_email().ok_or_else(|| denied("Verified sandbox owner unavailable"))?;
        if environment.scope != *scope || !environment.owner_principal.eq_ignore_ascii_case(&owner) {
            return Err(denied("Sandbox owner or scope changed"));
        }
        Ok(())
    }

    /// Host-only launch context. Unsupported managed routes fail rather than inheriting credentials.
    pub async fn prepare_inference(&self, session_id: &str, harness: HarnessId,
        model: Option<&str>, account_id: Option<&str>) -> Result<InferenceRoute, EngineError> {
        self.identity().map_err(|error| EngineError::Other(error.to_string()))?;
        if self.auth.device_mode() {
            let (_, scope) = self.identity().map_err(|error| EngineError::Other(error.to_string()))?;
            if scope.session_id.as_deref() != Some(session_id) || harness != HarnessId::Omp {
                return Err(EngineError::Other("Sandbox inference is bound to its OMP session".into()));
            }
        }
        self.relay.prepare(session_id, harness, model, account_id).await?
            .ok_or_else(|| EngineError::Other("This harness/model is unavailable in Crew-managed mode".into()))
    }

    pub fn retire_inference(&self, route: &InferenceRoute) { self.relay.remove(&route.token); }

    pub async fn control(&self, command_id: &str, control: Control, cancellation: &CancellationToken)
        -> Result<ControlOutcome, RpcError> {
        validate_id(command_id)?;
        let principal_id = self.validate_scope(control_scope(&control))?;
        let command = Command { principal_id, control };
        let bytes = serde_json::to_vec(&command).map_err(failed)?;
        let _gate = self.control_gate.lock().await;
        if !self.store.claim_crew_command(command_id, &bytes).map_err(failed)? {
            return self.outcome(command_id);
        }
        let scope = control_scope(&command.control);
        let before = if !matches!(command.control, Control::Inspect { .. })
            && let Some(id) = sandbox_id(&command.control) {
            if !matches!(command.control, Control::Stop { .. } | Control::Inspect { .. })
                && self.retired(id, scope)? {
                return self.finish(command_id, None, Some("Sandbox recovery was retired by Stop".into()));
            }
            match self.runtime.client().inspect(id, scope, cancellation).await {
                Ok(environment) => {
                    if let Err(error) = self.validate_environment(&environment, scope) {
                        return self.finish(command_id, None, Some(error.to_string()));
                    }
                    Some(environment)
                }
                Err(error) => return self.record_error(command_id, error),
            }
        } else { None };
        if let Some(environment) = &before {
            self.store.claim_crew_command(&format!("{command_id}:binding"),
                &serde_json::to_vec(environment).map_err(failed)?).map_err(failed)?;
        }
        if matches!(command.control, Control::Stop { .. }) {
            self.retire_pending(command_id, &command)?;
        }
        let result = match self.runtime.control(command.control.clone(), cancellation).await {
            Ok(mut result) => {
                self.validate_environment(&result.environment, scope)?;
                if let Some(before) = before.as_ref() {
                    validate_transition(before, &result.environment)?;
                }
                // A mutation response is not proof of readiness. Inspect the authoritative state.
                if !matches!(command.control, Control::Inspect { .. })
                    && let Some(id) = environment_id(&result.environment) {
                    match self.runtime.client().inspect(id, scope, cancellation).await {
                        Ok(observed) => {
                            self.validate_environment(&observed, scope)?;
                            validate_transition(&result.environment, &observed)?;
                            result.environment = observed;
                        }
                        Err(error) => return self.record_error(command_id, error),
                    }
                }
                result
            }
            Err(error) => return self.record_error(command_id, error),
        };
        if matches!(command.control, Control::Attach { .. }) {
            return match self.runtime.observe_attachment(result.environment, cancellation).await.map_err(failed)? {
                Some(mut observed) => {
                    observed.run_id = result.run_id;
                    self.finish(command_id, Some(observed), None)
                }
                None => Ok(unknown(command_id, "Native host authority is not yet confirmed for the observed lifecycle")),
            };
        }
        if !operation_observed(&command.control, &result.environment) {
            return Ok(unknown(command_id, "Platform accepted the operation; outcome is not yet confirmed"));
        }
        self.finish(command_id, Some(result), None)
    }

    fn finish(&self, id: &str, result: Option<ControlResult>, error: Option<String>) -> Result<ControlOutcome, RpcError> {
        let outcome = ControlOutcome { command_id: id.into(), status: if error.is_some() { "rejected" } else { "confirmed" }.into(), result, error };
        self.store.finish_crew_command(id, &serde_json::to_vec(&outcome).map_err(failed)?).map_err(failed)?;
        Ok(outcome)
    }

    fn record_error(&self, id: &str, error: ScaffoldError) -> Result<ControlOutcome, RpcError> {
        // No retry for ambiguous mutation delivery (including cancellation or malformed replies).
        if matches!(&error, ScaffoldError::AttachRejected(_))
            || matches!(&error, ScaffoldError::Api(api) if (400..500).contains(&api.0.status) && api.0.status != 408 && api.0.status != 429) {
            self.finish(id, None, Some(error.to_string()))
        } else { Ok(unknown(id, &error.to_string())) }
    }

    pub fn outcome(&self, command_id: &str) -> Result<ControlOutcome, RpcError> {
        let (principal, _) = self.controller()?;
        let (request, outcome) = self.store.crew_command(command_id).map_err(failed)?
            .ok_or_else(|| RpcError::BadParams("Unknown Crew command".into()))?;
        let command: Command = serde_json::from_slice(&request).map_err(failed)?;
        if command.principal_id != principal { return Err(denied("Command belongs to another owner")); }
        self.validate_scope(control_scope(&command.control))?;
        outcome.map(|bytes| serde_json::from_slice(&bytes).map_err(failed))
            .unwrap_or_else(|| Ok(unknown(command_id, "Command delivery is pending or its outcome is unknown; reconcile without replay")))
    }

    /// Reconcile only by observation. Never resubmit an unknown create, attach, or exec.
    pub async fn reconcile(&self, command_id: &str, cancellation: &CancellationToken) -> Result<ControlOutcome, RpcError> {
        let _gate = self.control_gate.lock().await;
        let old = self.outcome(command_id)?;
        if old.status != "unknown" { return Ok(old); }
        let (request, _) = self.store.crew_command(command_id).map_err(failed)?.expect("outcome read command");
        let command: Command = serde_json::from_slice(&request).map_err(failed)?;
        let scope = control_scope(&command.control);
        let environment = if let Some(id) = sandbox_id(&command.control) {
            if !matches!(command.control, Control::Stop { .. }) && self.retired(id, scope)? {
                return self.finish(command_id, None, Some("Recovery retired by Stop".into()));
            }
            self.runtime.client().inspect(id, scope, cancellation).await.map_err(failed)?
        } else {
            let matches = self.runtime.client().list(scope, cancellation).await.map_err(failed)?;
            let mut matches = matches.into_iter().filter(|environment| environment.scope == *scope);
            let Some(environment) = matches.next() else { return Ok(old) };
            if matches.next().is_some() { return Err(denied("Multiple sandboxes match the command; explicit reconciliation required")); }
            environment
        };
        self.validate_environment(&environment, scope)?;
        if matches!(command.control, Control::Create { .. } | Control::Attach { .. })
            && let SessionEnvironmentSource::Scaffold { lifecycle, .. } = &environment.source
            && matches!(lifecycle, ScaffoldLifecycle::Failed | ScaffoldLifecycle::Stopped) {
            return self.finish(command_id, None, Some(format!(
                "Creation/attachment recovery retired because the sandbox is {lifecycle:?}; no command was replayed."
            )));
        }
        if let Some((binding, _)) = self.store.crew_command(&format!("{command_id}:binding")).map_err(failed)? {
            let before: SessionEnvironment = serde_json::from_slice(&binding).map_err(failed)?;
            validate_transition(&before, &environment)?;
        }
        if matches!(command.control, Control::Attach { .. }) {
            return match self.runtime.observe_attachment(environment, cancellation).await.map_err(failed)? {
                Some(result) => self.finish(command_id, Some(result), None),
                None => Ok(old),
            };
        }
        // A read-only inspect cannot prove an archive transfer. Route recovery
        // requires the authoritative route, not merely a running sandbox.
        if matches!(command.control, Control::HandoffOmpSession { .. })
            || matches!(&command.control, Control::UpdateAgentRoute { agent_route, .. }
                if environment.unknown.get("agentRoute")
                    .and_then(|value| serde_json::from_value::<AgentRoute>(value.clone()).ok()).as_ref() != Some(agent_route))
            || !operation_observed(&command.control, &environment) { return Ok(old); }
        let result = ControlResult { environment, preparation_generation: None, attached_device_id: None,
            run_id: None, room_projection: None, control_grant: None,
            handoff_native_session_id: None, handoff_cwd: None };
        self.finish(command_id, Some(result), None)
    }

    pub(crate) async fn reconcile_pending_batch(&self, after: &mut String) -> Result<Option<Vec<ControlResult>>, RpcError> {
        let (principal, _) = self.controller()?;
        let cancellation = CancellationToken::new();
        let commands = self.store.pending_crew_commands(after, 64).map_err(failed)?;
        if commands.is_empty() { return Ok(None); }
        let mut results = Vec::new();
        for (id, request) in commands {
            after.clone_from(&id);
            let Ok(command) = serde_json::from_slice::<Command>(&request) else { continue };
            if command.principal_id != principal { continue; }
            match tokio::time::timeout(std::time::Duration::from_secs(30), self.reconcile(&id, &cancellation)).await {
                Ok(Ok(outcome)) => if let Some(result) = outcome.result { results.push(result); },
                Ok(Err(error)) => tracing::warn!(command = %id, %error, "Crew recovery remains unconfirmed"),
                Err(_) => tracing::warn!(command = %id, "Crew observation timed out; no mutation replayed"),
            }
        }
        Ok(Some(results))
    }

    fn retired(&self, sandbox: &str, scope: &CollaborationScope) -> Result<bool, RpcError> {
        for (_, request, outcome) in self.store.crew_commands().map_err(failed)? {
            let Ok(command) = serde_json::from_slice::<Command>(&request) else { continue };
            if matches!(&command.control, Control::Stop { sandbox_id, scope: stopped } if sandbox_id == sandbox && stopped == scope) {
                let rejected = outcome.as_deref().and_then(|bytes| serde_json::from_slice::<ControlOutcome>(bytes).ok())
                    .is_some_and(|outcome| outcome.status == "rejected");
                if !rejected { return Ok(true); }
            }
        }
        Ok(false)
    }

    fn retire_pending(&self, current: &str, stopping: &Command) -> Result<(), RpcError> {
        for (id, request, outcome) in self.store.crew_commands().map_err(failed)? {
            if id == current || outcome.is_some() { continue; }
            let Ok(command) = serde_json::from_slice::<Command>(&request) else { continue };
            if control_scope(&command.control) == control_scope(&stopping.control)
                && (sandbox_id(&command.control).is_none() || sandbox_id(&command.control) == sandbox_id(&stopping.control)) {
                self.finish(&id, None, Some("Recovery retired by Stop".into()))?;
            }
        }
        Ok(())
    }

    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let cancellation = CancellationToken::new();
        match method {
            "crew.status" => {
                let (principal, mut scope) = self.identity()?;
                if scope.deployment_id.is_none() { scope.deployment_id = Some(self.runtime.deployment_id().into()); }
                Ok(json!({"principalId":principal,"scope":scope,"managed":true,
                    "role":if self.auth.device_mode(){"scaffold_host"}else{"local_controller"},
                    "runtimeVersion":crate::scaffold::SCAFFOLD_COMET_RUNTIME_VERSION.trim()}))
            }
            "scaffold.refresh" => {
                let scope: CollaborationScope = serde_json::from_value(params).map_err(bad)?;
                let mut validation_scope = scope.clone();
                validation_scope.session_id.get_or_insert_with(|| "list".into());
                self.validate_scope(&validation_scope)?;
                let snapshot = self.runtime.refresh(&scope, &cancellation).await.map_err(failed)?;
                for environment in &snapshot.environments { self.validate_environment(environment, &environment.scope)?; }
                serde_json::to_value(snapshot).map_err(failed)
            }
            "scaffold.control" => {
                #[derive(Deserialize)] #[serde(rename_all="camelCase")]
                struct Input { command_id: String, control: Control }
                let input: Input = serde_json::from_value(params).map_err(bad)?;
                serde_json::to_value(self.control(&input.command_id,input.control,&cancellation).await?).map_err(failed)
            }
            "scaffold.outcome" | "scaffold.reconcile" => {
                let id = string(&params,"commandId")?;
                let outcome = if method.ends_with("reconcile") { self.reconcile(id,&cancellation).await? } else { self.outcome(id)? };
                serde_json::to_value(outcome).map_err(failed)
            }
            "crew.directory.get" => {
                self.controller()?;
                let deployment = params.get("deploymentId").and_then(Value::as_str).unwrap_or(self.runtime.deployment_id());
                if deployment != self.runtime.deployment_id() { return Err(denied("Wrong directory deployment")); }
                self.runtime.client().get_crew_session(string(&params,"sessionId")?,Some(deployment)).await.map_err(failed)
            }
            "crew.accounts.list" => { self.controller()?; Ok(json!({"accounts":self.runtime.client().list_agent_accounts().await.map_err(failed)?})) }
            "crew.accounts.revoke" => { self.controller()?; self.runtime.client().revoke_agent_account(string(&params,"accountId")?).await.map_err(failed)?; Ok(json!({})) }
            "crew.route.receipt" => { self.controller()?; serde_json::to_value(self.runtime.client().get_agent_route_receipt(string(&params,"sessionId")?,&cancellation).await.map_err(failed)?).map_err(failed) }
            _ => Err(RpcError::UnknownMethod(method.into())),
        }
    }

    /// Local IPC only; never register this on a peer/device RPC service.
    pub async fn import_local_account(&self, accounts: &crate::agent_accounts::AgentAccounts,
        harness: HarnessId, account_id: &str) -> Result<Value, RpcError> {
        self.controller()?;
        serde_json::to_value(accounts.import_local_shared(&self.runtime.client(),harness,account_id).await.map_err(failed)?).map_err(failed)
    }

    /// Prepare a completed-turn native fork. Keep the source writer alive;
    /// publish the target and durably admit the original immutable command ID.
    pub async fn prepare_handoff(&self, request: HandoffRequest, cancellation: &CancellationToken) -> Result<HandoffReceipt, RpcError> {
        validate_id(&request.command_id)?;
        if request.prompt.trim().is_empty() || request.native_session_id.trim().is_empty() || request.cwd.trim().is_empty() {
            return Err(RpcError::BadParams("Native handoff requires prompt, native identity and cwd".into()));
        }
        let (principal, mut scope) = self.controller()?;
        let root_id = format!("crew-handoff:{}",request.command_id);
        let immutable = serde_json::to_vec(&json!({"principal":principal,"request":request})).map_err(failed)?;
        // Root claim contains every immutable input, including prompt and recovery IDs.
        self.store.claim_crew_command(&root_id,&immutable).map_err(failed)?;
        if let Some((_,Some(receipt))) = self.store.crew_command(&root_id).map_err(failed)? {
            let receipt: HandoffReceipt = serde_json::from_slice(&receipt).map_err(failed)?;
            if self.retired(&receipt.sandbox_id, &receipt.environment.scope)? {
                return Err(denied("Stopped handoff cannot recover"));
            }
            self.validate_environment(&receipt.environment, &receipt.environment.scope)?;
            return Ok(receipt);
        }
        let chat_id = request.recover_chat_id.clone().unwrap_or_else(|| {
            use sha2::{Digest,Sha256};
            let digest = Sha256::digest(root_id.as_bytes());
            let mut bytes = [0u8;16]; bytes.copy_from_slice(&digest[..16]);
            bytes[6]=(bytes[6]&0x0f)|0x80; bytes[8]=(bytes[8]&0x3f)|0x80;
            uuid::Uuid::from_bytes(bytes).to_string()
        });
        scope.deployment_id=Some(self.runtime.deployment_id().into()); scope.session_id=Some(chat_id.clone());
        let sandbox = if let Some(id) = request.recover_sandbox_id.as_ref() {
            if request.recover_chat_id.is_none() { return Err(bad("Recovery requires both chat and sandbox IDs")); }
            let env = self.runtime.client().inspect(id,&scope,cancellation).await.map_err(failed)?;
            self.validate_environment(&env,&scope)?;
            if self.retired(id,&scope)? { return Err(denied("Stopped handoff cannot recover")); }
            id.clone()
        } else {
            let create_id = format!("{root_id}:create");
            let mut create = self.control(&create_id,Control::Create {
                scope:scope.clone(),name:Some("Crew handoff".into()),source_ref:None,region:None,
                database_environment:request.database_environment,agent_route:request.agent_route.clone(),
            },cancellation).await?;
            if create.status == "unknown" { create = self.reconcile(&create_id, cancellation).await?; }
            let created = confirmed(create)?;
            environment_id(&created.environment).ok_or_else(|| failed("Created environment has no sandbox"))?.into()
        };
        // Attach installs the verified runtime and releases its startup marker;
        // waiting for application readiness first would strand a fresh sandbox.
        let attach_id = format!("{root_id}:attach");
        let mut attachment = self.control(&attach_id,Control::Attach {
            sandbox_id:sandbox.clone(),scope:scope.clone(),
        },cancellation).await?;
        if attachment.status == "unknown" { attachment = self.reconcile(&attach_id, cancellation).await?; }
        let attachment = confirmed(attachment)?;
        let ready = async {
            loop {
                let environment = self.runtime.client().inspect(&sandbox, &scope, cancellation).await.map_err(failed)?;
                self.validate_environment(&environment, &scope)?;
                match environment.source {
                    SessionEnvironmentSource::Scaffold { lifecycle: ScaffoldLifecycle::Ready | ScaffoldLifecycle::AgentRunning, .. } => return Ok::<(), RpcError>(()),
                    SessionEnvironmentSource::Scaffold { lifecycle: ScaffoldLifecycle::Stopped | ScaffoldLifecycle::Failed | ScaffoldLifecycle::Paused, .. } => return Err(failed("Handoff target is not running")),
                    _ => {}
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        };
        tokio::select! {
            _ = cancellation.cancelled() => return Err(failed("Handoff preparation cancelled")),
            result = tokio::time::timeout(std::time::Duration::from_secs(300), ready) =>
                result.map_err(|_| failed("Preserved sandbox is still starting; retry preparation with the same command ID"))??,
        }
        let transferred = confirmed(self.control(&format!("{root_id}:transfer"),Control::HandoffOmpSession {
            sandbox_id:sandbox.clone(),scope:scope.clone(),native_session_id:request.native_session_id.clone(),cwd:request.cwd,
        },cancellation).await?)?;
        let receipt = HandoffReceipt {chat_id,sandbox_id:sandbox,command_id:request.command_id,
            environment:attachment.environment.clone(),attachment,
            native_session_id:transferred.handoff_native_session_id.ok_or_else(|| failed("Missing transferred native identity"))?,
            cwd:transferred.handoff_cwd.ok_or_else(|| failed("Missing transferred worktree"))?};
        self.store.finish_crew_command(&root_id,&serde_json::to_vec(&receipt).map_err(failed)?).map_err(failed)?;
        Ok(receipt)
    }

    /// Serialize the final durable admission with Stop on this controller.
    /// Re-inspect the same sandbox/epoch; another controller may have stopped it.
    pub async fn admit_handoff<T, F>(&self, receipt: &HandoffReceipt, admit: F) -> Result<T, RpcError>
    where T: Send + 'static, F: FnOnce() -> Result<T, RpcError> + Send + 'static {
        let gate = self.control_gate.clone().lock_owned().await;
        self.validate_environment(&receipt.environment, &receipt.environment.scope)?;
        let environment = self.runtime.client().inspect(
            &receipt.sandbox_id, &receipt.environment.scope, &CancellationToken::new(),
        ).await.map_err(failed)?;
        self.validate_environment(&environment, &receipt.environment.scope)?;
        match (&receipt.environment.source, &environment.source) {
            (SessionEnvironmentSource::Scaffold { lifecycle_epoch: before, .. },
                SessionEnvironmentSource::Scaffold { lifecycle: ScaffoldLifecycle::Ready | ScaffoldLifecycle::AgentRunning, lifecycle_epoch: after, .. })
                if before.is_some() && before == after => {}
            _ => return Err(denied("Handoff target is stopped, paused, or changed epoch; do not replay its prompt")),
        }
        let services = self.clone();
        let receipt = receipt.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = gate;
            services.validate_environment(&receipt.environment, &receipt.environment.scope)?;
            if services.retired(&receipt.sandbox_id, &receipt.environment.scope)? {
                return Err(denied("Stopped handoff cannot admit a command"));
            }
            admit()
        }).await.map_err(failed)?
    }
}

fn control_scope(control:&Control)->&CollaborationScope {
    match control { Control::Inspect{scope,..}|Control::Create{scope,..}|Control::Pause{scope,..}|
        Control::Resume{scope,..}|Control::Stop{scope,..}|Control::Attach{scope,..}|
        Control::UpdateAgentRoute{scope,..}|Control::HandoffOmpSession{scope,..}=>scope }
}
fn sandbox_id(control:&Control)->Option<&str> {
    match control { Control::Create{..}=>None,Control::Inspect{sandbox_id,..}|Control::Pause{sandbox_id,..}|
        Control::Resume{sandbox_id,..}|Control::Stop{sandbox_id,..}|Control::Attach{sandbox_id,..}|
        Control::UpdateAgentRoute{sandbox_id,..}|Control::HandoffOmpSession{sandbox_id,..}=>Some(sandbox_id) }
}
fn environment_id(environment:&SessionEnvironment)->Option<&str> {
    match &environment.source { SessionEnvironmentSource::Scaffold{sandbox_id,..}=>Some(sandbox_id),_=>None }
}
fn validate_transition(before:&SessionEnvironment,after:&SessionEnvironment)->Result<(),RpcError> {
    if before.scope!=after.scope || before.owner_principal!=after.owner_principal || environment_id(before)!=environment_id(after) {
        return Err(denied("Sandbox identity changed during operation"));
    }
    if let (SessionEnvironmentSource::Scaffold{lifecycle_epoch:Some(old),..},SessionEnvironmentSource::Scaffold{lifecycle_epoch:Some(new),..})=(&before.source,&after.source) {
        if new<old { return Err(denied("Stale sandbox lifecycle completion")); }
    }
    Ok(())
}
fn operation_observed(control:&Control,environment:&SessionEnvironment)->bool {
    let SessionEnvironmentSource::Scaffold{lifecycle,..}=&environment.source else {return false};
    match control {
        Control::Pause{..}=>*lifecycle==ScaffoldLifecycle::Paused,
        Control::Stop{..}=>*lifecycle==ScaffoldLifecycle::Stopped,
        Control::Resume{..}=>matches!(lifecycle,ScaffoldLifecycle::Ready|ScaffoldLifecycle::AgentRunning),
        Control::Attach{..}=>false, // Attachment requires native authority, not application health.
        Control::Create{..}=>!matches!(lifecycle,ScaffoldLifecycle::Stopped|ScaffoldLifecycle::Failed),
        _=>true,
    }
}
fn confirmed(outcome:ControlOutcome)->Result<ControlResult,RpcError> {
    if outcome.status!="confirmed" {return Err(failed(outcome.error.unwrap_or_else(|| "Handoff outcome unknown; reconcile without repeating creation".into())));}
    outcome.result.ok_or_else(||failed("Confirmed command has no result"))
}
fn unknown(id:&str,error:&str)->ControlOutcome { ControlOutcome{command_id:id.into(),status:"unknown".into(),result:None,error:Some(error.into())} }
fn validate_id(id:&str)->Result<(),RpcError> { if id.trim().is_empty()||id.len()>512 {Err(bad("Invalid immutable command ID"))}else{Ok(())} }
fn string<'a>(value:&'a Value,key:&str)->Result<&'a str,RpcError>{value.get(key).and_then(Value::as_str).filter(|v|!v.trim().is_empty()).ok_or_else(||bad(format!("Missing {key}")))}
fn denied(message:impl ToString)->RpcError{RpcError::Capability(message.to_string())}
fn failed(message:impl ToString)->RpcError{RpcError::Failed(message.to_string())}
fn bad(message:impl ToString)->RpcError{RpcError::BadParams(message.to_string())}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_startup_is_not_a_confirmed_resume() {
        let scope=CollaborationScope{project_id:"p".into(),deployment_id:Some("d".into()),session_id:Some("s".into()),unknown:Default::default()};
        let mut environment=SessionEnvironment{source:SessionEnvironmentSource::Scaffold{sandbox_id:"box".into(),region:None,lifecycle:ScaffoldLifecycle::Resuming,lifecycle_epoch:Some(2),links:Default::default()},name:None,owner_principal:"owner@example.com".into(),scope:scope.clone(),source_ref:None,last_activity_at:None,database_environment:None,unknown:Default::default()};
        assert!(!operation_observed(&Control::Resume{sandbox_id:"box".into(),scope:scope.clone()},&environment));
        let previous=environment.clone();
        if let SessionEnvironmentSource::Scaffold{lifecycle,lifecycle_epoch,..}=&mut environment.source{*lifecycle=ScaffoldLifecycle::Ready;*lifecycle_epoch=Some(1);}
        assert!(validate_transition(&previous,&environment).is_err());
        environment.owner_principal="other@example.com".into();
        assert!(validate_transition(&previous,&environment).is_err());
    }
}
