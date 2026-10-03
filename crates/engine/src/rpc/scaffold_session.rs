//! Native Scaffold provisioning shared by the composer and agent-facing CLI.

use super::*;
use comet_harness::CancellationToken;
use comet_proto::{
    AgentRoute, AgentSessionSource, RunRequest, SandboxLevel, ScaffoldDatabaseEnvironment,
    ScaffoldLifecycle,
};
use comet_rpc::{HandoffSessionToScaffoldParams, HandoffSessionToScaffoldResult};

const TRANSIENT_FAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PrepareScaffoldSessionParams {
    scope: CollaborationScope,
    name: Option<String>,
    source_ref: Option<String>,
    #[serde(default)]
    database_environment: ScaffoldDatabaseEnvironment,
    agent_route: AgentRoute,
    omp_handoff: Option<OmpHandoff>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OmpHandoff {
    native_session_id: String,
    cwd: String,
}

impl EngineRpc {
    pub(super) async fn control_scaffold_environment(
        &self,
        control: ScaffoldEnvironmentControl,
        cancellation: &CancellationToken,
    ) -> Result<ScaffoldEnvironmentControlResult, RpcError> {
        let scaffold = self.scaffold()?;
        let owner_room = self.prepare_scaffold_attach(&control)?;
        self.await_scaffold_owner_room(owner_room.as_deref(), cancellation)
            .await?;
        let result = scaffold
            .control(control, cancellation)
            .await
            .map_err(scaffold_control_error)?;
        if let Some(projection) = result.room_projection.as_ref() {
            if result.environment.scope.project_id != projection.project_id
                || result.environment.scope.deployment_id.as_deref()
                    != Some(projection.deployment_id.as_str())
                || result.environment.scope.session_id.as_deref()
                    != Some(projection.session_id.as_str())
            {
                return Err(RpcError::Failed(
                    "Scaffold attachment environment projection mismatch".into(),
                ));
            }
            self.workspace
                .upsert_session_ref(&projection.session_id, Some(result.environment.clone()))
                .map_err(|error| RpcError::Failed(error.to_string()))?;
        }
        if let Err(error) = self.install_scaffold_control_grant(&result) {
            tracing::warn!(error = %error, "Scaffold attached without local control grant projection");
        }
        Ok(result)
    }

    pub(super) async fn prepare_scaffold_session(
        &self,
        params: PrepareScaffoldSessionParams,
    ) -> Result<ScaffoldEnvironmentControlResult, RpcError> {
        let _preparation = self.scaffold()?.preparation_gate(&params.scope)
            .try_lock_owned().map_err(|_| RpcError::Failed(
                "Scaffold session preparation already in progress; wait for the existing request".into()
            ))?;
        self.prepare_scaffold_session_locked(params, None, None)
            .await
    }

    // The caller holds the scope gate through native transfer and command admission.
    async fn prepare_scaffold_session_locked(
        &self,
        params: PrepareScaffoldSessionParams,
        recovery_sandbox: Option<&str>,
        native_source: Option<&Chat>,
    ) -> Result<ScaffoldEnvironmentControlResult, RpcError> {
        self.scaffold()?;
        let state = self.auth()?.state();
        let project = state
            .project_scope()
            .ok_or_else(|| RpcError::Failed("authenticated project scope unavailable".into()))?;
        if params.scope.project_id != project
            || params.scope.project_id != self.workspace.project_scope()
        {
            return Err(RpcError::Failed(
                "Scaffold preparation project mismatch".into(),
            ));
        }
        if params.scope.deployment_id.as_deref() != Some(self.scaffold()?.deployment_id()) {
            return Err(RpcError::Failed(
                "Scaffold preparation deployment mismatch".into(),
            ));
        }
        if params
            .scope
            .session_id
            .as_deref()
            .and_then(canonical_session_id)
            .is_none()
        {
            return Err(RpcError::Failed(
                "Scaffold preparation requires a Crew session UUID".into(),
            ));
        }
        let session_id = params
            .scope
            .session_id
            .as_deref()
            .expect("validated session id");
        let actor = state
            .user()
            .ok_or_else(|| RpcError::Failed("authenticated local identity unavailable".into()))?;
        params
            .agent_route
            .validate()
            .map_err(|error| RpcError::Failed(error.into()))?;
        let reference = self
            .workspace
            .doc()
            .session_ref(&actor.id, session_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        if let Some(sandbox_id) = recovery_sandbox {
            let environment = reference
                .as_ref()
                .and_then(|reference| reference.environment.as_ref())
                .ok_or_else(|| {
                    recovery_error(
                        "accepted target is missing from the signed-in owner's session references",
                    )
                })?;
            validate_recovery_environment(
                environment,
                sandbox_id,
                &params.scope,
                params.database_environment,
            )?;
            if environment.owner_principal != actor.id && environment.owner_principal != actor.email
            {
                return Err(recovery_error(
                    "accepted target belongs to a different owner",
                ));
            }
            self.require_unadmitted_handoff(session_id)?;
        }
        if reference.as_ref().is_some_and(|reference| {
            reference.environment.is_none()
                && reference.startup.as_ref().is_some_and(|startup| {
                    startup.status == comet_proto::SessionStartupStatus::CreationUncertain
                })
        }) {
            return Err(RpcError::Failed(
                "Sandbox creation outcome is unknown; inspect the existing request before retrying creation".into(),
            ));
        }
        let accepted = reference
            .and_then(|reference| reference.environment)
            .filter(|environment| {
                matches!(
                    &environment.source,
                    SessionEnvironmentSource::Scaffold { .. }
                ) && environment.scope == params.scope
            });
        // Explicit recovery can never reach Create, even if its reference was removed.
        if recovery_sandbox.is_some() && accepted.is_none() {
            return Err(recovery_error(
                "accepted target no longer matches this session scope",
            ));
        }
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        if let Some(sandbox_id) = recovery_sandbox {
            self.validate_recovery_route(sandbox_id, &params.scope, &params.agent_route, &cancellation)
                .await?;
        }
        let mut startup = PreparationOutcome::begin(self.workspace.clone(), session_id)?;
        let expected_scope = params.scope.clone();
        let expected_database = params.database_environment;
        let expected_route = params.agent_route.clone();
        let startup_state = &startup.startup;
        let attached_device = std::sync::Mutex::new(None::<String>);
        let mut result = prepare_scaffold_session_with(params, accepted, |control| {
            let expected_scope = &expected_scope;
            let cancellation = &cancellation;
            let expected_route = &expected_route;
            let attached_device = &attached_device;
            async move {
                let creating = matches!(&control, ScaffoldEnvironmentControl::Create { .. });
                let attaching = matches!(&control, ScaffoldEnvironmentControl::Attach { .. });
                let importing = matches!(&control, ScaffoldEnvironmentControl::HandoffOmpSession { .. });
                if let Some(sandbox_id) = recovery_sandbox {
                    if creating {
                        return Err(recovery_error("refusing to create a replacement target"));
                    }
                    self.require_unadmitted_handoff(expected_scope.session_id.as_deref().unwrap())?;
                    if matches!(&control, ScaffoldEnvironmentControl::HandoffOmpSession { .. }) {
                        self.validate_recovery_route(sandbox_id, expected_scope, expected_route, cancellation).await?;
                    }
                    // Check the persisted owner reference again before each external control.
                    let current = self.workspace.doc().session_ref(&actor.id, expected_scope.session_id.as_deref().unwrap())
                        .map_err(|error| RpcError::Failed(error.to_string()))?
                        .and_then(|reference| reference.environment)
                        .ok_or_else(|| recovery_error("accepted target disappeared; no replacement will be created"))?;
                    validate_recovery_environment(&current, sandbox_id, expected_scope, expected_database)?;
                    if current.owner_principal != actor.id && current.owner_principal != actor.email {
                        return Err(recovery_error("accepted target owner changed"));
                    }
                }
                if creating {
                    let mut uncertain = startup_state.clone();
                    uncertain.status = comet_proto::SessionStartupStatus::CreationUncertain;
                    self.workspace.update_session_startup(
                        expected_scope.session_id.as_deref().unwrap(),
                        Some(&uncertain.generation.clone()), uncertain,
                    ).map_err(|error| RpcError::Failed(error.to_string()))?;
                }
                let result = match self.control_scaffold_environment(control, cancellation).await {
                    Ok(result) => result,
                    Err(error) => {
                        // This error is raised before HTTP dispatch, unlike a lost response.
                        if creating && matches!(&error, RpcError::ScaffoldAuthUnavailable) {
                            self.workspace.update_session_startup(
                                expected_scope.session_id.as_deref().unwrap(),
                                Some(&startup_state.generation), startup_state.clone(),
                            ).map_err(|error| RpcError::Failed(error.to_string()))?;
                        }
                        return Err(error);
                    }
                };
                if let Some(sandbox_id) = recovery_sandbox {
                    validate_recovery_environment(&result.environment, sandbox_id, expected_scope, expected_database)?;
                    if result.environment.owner_principal != actor.id && result.environment.owner_principal != actor.email {
                        return Err(recovery_error("remote target owner changed"));
                    }
                }
                if attaching {
                    *attached_device.lock().unwrap_or_else(|error| error.into_inner()) = result.attached_device_id.clone();
                }
                if importing {
                    if let Some(source) = native_source {
                        let device = attached_device.lock().unwrap_or_else(|error| error.into_inner());
                        let device = device.as_deref().ok_or_else(|| recovery_error("attachment device disappeared after native import"))?;
                        self.persist_handoff_context(source, expected_route.omp_model(), device, &result)?;
                    }
                }
                if creating {
                    let SessionEnvironmentSource::Scaffold { sandbox_id, .. } = &result.environment.source else {
                        return Err(RpcError::Failed("Scaffold returned a local environment".into()));
                    };
                    if &result.environment.scope != expected_scope {
                        return Err(RpcError::Failed(format!("Scaffold creation returned a different session scope; sandbox {sandbox_id}")));
                    }
                    // Preserve the accepted target even if attach or transfer fails.
                    // Repeated preparation for this session must recover, not create.
                    self.workspace.upsert_session_ref(expected_scope.session_id.as_deref().unwrap(), Some(result.environment.clone()))
                        .map_err(|error| RpcError::Failed(format!("{error}; sandbox {sandbox_id}; do not retry creation blindly")))?;
                    self.workspace.update_session_startup(
                        expected_scope.session_id.as_deref().unwrap(),
                        Some(&startup_state.generation), startup_state.clone(),
                    ).map_err(|error| RpcError::Failed(error.to_string()))?;
                }
                Ok(result)
            }
        }).await?;
        self.install_scaffold_control_grant(&result)?;
        self.workspace
            .upsert_session_ref(
                result
                    .environment
                    .scope
                    .session_id
                    .as_deref()
                    .expect("validated session identity"),
                Some(result.environment.clone()),
            )
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        result.preparation_generation = Some(startup.startup.generation.clone());
        startup.armed = false;
        Ok(result)
    }

    pub(super) async fn handoff_session_to_scaffold(
        &self,
        params: HandoffSessionToScaffoldParams,
    ) -> Result<HandoffSessionToScaffoldResult, RpcError> {
        self.handoff_session_to_scaffold_with(params, None).await
    }

    pub(super) async fn recover_session_handoff_to_scaffold(
        &self,
        params: comet_rpc::RecoverSessionHandoffToScaffoldParams,
    ) -> Result<HandoffSessionToScaffoldResult, RpcError> {
        let chat_id = canonical_session_id(&params.recover_chat_id)
            .ok_or_else(|| recovery_error("--recover-chat-id must be a Crew session UUID"))?;
        if params.recover_sandbox_id.trim().is_empty() || params.recover_sandbox_id.len() > 256 {
            return Err(recovery_error(
                "--recover-sandbox-id must identify the preserved sandbox",
            ));
        }
        self.handoff_session_to_scaffold_with(
            params.handoff,
            Some((chat_id, params.recover_sandbox_id)),
        )
        .await
    }

    async fn handoff_session_to_scaffold_with(
        &self,
        params: HandoffSessionToScaffoldParams,
        recovery: Option<(String, String)>,
    ) -> Result<HandoffSessionToScaffoldResult, RpcError> {
        self.require_session_import()?;
        if self.runtime_profile != RuntimeProfile::LocalController {
            return Err(RpcError::Failed(
                "native_handoff_requires_local_controller".into(),
            ));
        }
        self.scaffold()?;
        if params.prompt.trim().is_empty() || params.prompt.len() > 1024 * 1024 {
            return Err(RpcError::Failed("native_handoff_prompt_invalid".into()));
        }
        let source_id = canonical_session_id(&params.source_chat_id)
            .ok_or_else(|| RpcError::Failed("invalid_source_chat_id".into()))?;
        let source = self
            .workspace
            .doc()
            .chat(&source_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?
            .ok_or_else(|| RpcError::Failed("source_session_not_found".into()))?;
        if !self.doc_host.is_locally_hosted(&source_id) {
            return Err(RpcError::Failed("source_session_not_hosted_here".into()));
        }
        let auth_state = self.auth()?.state();
        let actor_subject = auth_state
            .user()
            .ok_or_else(|| RpcError::Failed("authenticated local identity unavailable".into()))?
            .id
            .clone();
        if self
            .workspace
            .doc()
            .session_ref(&actor_subject, &source_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?
            .and_then(|session_ref| session_ref.environment)
            .is_some_and(|environment| {
                matches!(
                    environment.source,
                    SessionEnvironmentSource::Scaffold { .. }
                )
            })
        {
            return Err(RpcError::Failed("source_session_already_scaffold".into()));
        }
        let config = source
            .config
            .as_ref()
            .filter(|config| config.harness == HarnessId::Omp)
            .ok_or_else(|| RpcError::Failed("native_handoff_requires_omp".into()))?;
        let (native_session_id, cwd) = source
            .harness_session_id
            .as_ref()
            .zip(source.harness_session_cwd.as_ref())
            .filter(|(id, cwd)| !id.trim().is_empty() && !cwd.trim().is_empty())
            .ok_or_else(|| RpcError::Failed("native_handoff_source_context_missing".into()))?;
        let model = config
            .model
            .as_deref()
            .and_then(crate::local_sessions::canonical_omp_model_selector)
            .ok_or_else(|| RpcError::Failed("native_handoff_model_missing".into()))?;
        let agent_route = AgentRoute::from_omp_model(&model)
            .ok_or_else(|| RpcError::Failed("native_handoff_model_unsupported".into()))?;
        let space_id = source
            .space_id
            .as_deref()
            .ok_or_else(|| RpcError::Failed("native_handoff_source_folder_missing".into()))?;
        if self
            .workspace
            .doc()
            .space(space_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?
            .is_none()
        {
            return Err(RpcError::Failed(
                "native_handoff_source_folder_missing".into(),
            ));
        }
        let project_id = auth_state
            .project_scope()
            .ok_or_else(|| RpcError::Failed("authenticated project scope unavailable".into()))?
            .to_string();
        let chat_id = recovery
            .as_ref()
            .map_or_else(crate::new_id, |(chat_id, _)| chat_id.clone());
        if chat_id == source_id {
            return Err(recovery_error(
                "source and recovery target must be different sessions",
            ));
        }
        let scope = CollaborationScope {
            project_id: project_id.clone(),
            deployment_id: Some(self.scaffold()?.deployment_id().to_string()),
            session_id: Some(chat_id.clone()),
            unknown: Default::default(),
        };
        let remote_model = agent_route.omp_model();
        let _preparation = self.scaffold()?.preparation_gate(&scope)
            .try_lock_owned().map_err(|_| RpcError::Failed(
                "Scaffold session preparation already in progress; wait for the existing request".into()
            ))?;
        let recovery_sandbox = recovery.as_ref().map(|(_, sandbox_id)| sandbox_id.as_str());
        let attached = self
            .prepare_scaffold_session_locked(
                PrepareScaffoldSessionParams {
                    scope,
                    name: source.title.clone(),
                    source_ref: Some("master".into()),
                    database_environment: params.database_environment,
                    agent_route,
                    omp_handoff: Some(OmpHandoff {
                        native_session_id: native_session_id.clone(),
                        cwd: cwd.clone(),
                    }),
                },
                recovery_sandbox,
                Some(&source),
            )
            .await?;
        self.admit_scaffold_handoff(source, params.prompt, remote_model, actor_subject, attached)
    }

    fn require_unadmitted_handoff(&self, session_id: &str) -> Result<(), RpcError> {
        self.require_no_handoff_command(session_id)?;
        if self
            .workspace
            .doc()
            .chat(session_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?
            .is_some_and(|chat| {
                chat.harness_session_id.is_some() || chat.harness_session_cwd.is_some()
            })
        {
            return Err(recovery_error(
                "target already imported native context; inspect the existing chat instead of replaying handoff",
            ));
        }
        Ok(())
    }

    fn require_no_handoff_command(&self, session_id: &str) -> Result<(), RpcError> {
        let reference = self
            .workspace
            .session_startup(session_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        if reference.is_some_and(|startup| {
            startup.status == comet_proto::SessionStartupStatus::Admitted
                || startup.command_id.is_some()
        }) || self.target_has_start_command(session_id)?
            || self
                .sessions
                .session_status(session_id)
                .is_some_and(|session| {
                    matches!(
                        session.status,
                        SessionStatus::Working | SessionStatus::AwaitingInput
                    )
                })
        {
            return Err(recovery_error(
                "target already admitted a command or is active; inspect the existing chat instead of replaying handoff",
            ));
        }
        Ok(())
    }

    fn target_has_start_command(&self, session_id: &str) -> Result<bool, RpcError> {
        // Read only discriminants, not transcript-sized command payloads or the whole ledger.
        let handle = self
            .doc_host
            .open(session_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let commands = handle.doc().doc().get_list("commands");
        for index in 0..commands.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) =
                commands.get(index)
            else {
                continue;
            };
            let Some(loro::ValueOrContainer::Value(loro::LoroValue::Map(payload))) =
                row.get("payload")
            else {
                continue;
            };
            if matches!(payload.get("kind"), Some(loro::LoroValue::String(kind)) if kind.as_str() == "run")
            {
                return Ok(true);
            }
            if matches!(payload.get("kind"), Some(loro::LoroValue::String(kind)) if kind.as_str() == "control")
            {
                if let Some(loro::LoroValue::Map(action)) = payload.get("action") {
                    if matches!(action.get("action"), Some(loro::LoroValue::String(kind)) if kind.as_str() == "start")
                    {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    async fn validate_recovery_route(
        &self,
        sandbox_id: &str,
        scope: &CollaborationScope,
        expected: &AgentRoute,
        cancellation: &CancellationToken,
    ) -> Result<(), RpcError> {
        let (environment, route) = self.scaffold()?.client()
            .inspect_agent_route(sandbox_id, scope, cancellation).await
            .map_err(|error| recovery_error(&format!("cannot verify the preserved agent route: {error}")))?;
        let state = self.auth()?.state();
        let actor = state.user().ok_or_else(|| recovery_error("authenticated owner is unavailable"))?;
        if environment.owner_principal != actor.id && environment.owner_principal != actor.email {
            return Err(recovery_error("preserved agent route belongs to a different owner"));
        }
        if route != *expected
        {
            return Err(recovery_error(
                "preserved target's agent route differs from the source model; restore the original source model, do not create a replacement",
            ));
        }
        Ok(())
    }

    fn persist_handoff_context(
        &self,
        source: &Chat,
        remote_model: String,
        owner_device_id: &str,
        attached: &ScaffoldEnvironmentControlResult,
    ) -> Result<(), RpcError> {
        let chat_id = attached
            .environment
            .scope
            .session_id
            .as_deref()
            .ok_or_else(|| recovery_error("native import returned no target session"))?;
        let native_id = attached
            .handoff_native_session_id
            .as_ref()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| recovery_error("native import returned no context identity"))?;
        let remote_cwd = attached
            .handoff_cwd
            .as_ref()
            .filter(|cwd| !cwd.trim().is_empty())
            .ok_or_else(|| recovery_error("native import returned no context directory"))?;
        let config = source
            .config
            .as_ref()
            .ok_or_else(|| recovery_error("source configuration disappeared"))?;
        if let Some(existing) = self
            .workspace
            .doc()
            .chat(chat_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?
        {
            if existing.harness_session_id.as_deref() == Some(native_id.as_str())
                && existing.harness_session_cwd.as_deref() == Some(remote_cwd.as_str())
                && existing.device_id == owner_device_id
            {
                return Ok(());
            }
            if existing.harness_session_id.is_some() || existing.harness_session_cwd.is_some() {
                return Err(recovery_error(
                    "target already contains different native context",
                ));
            }
        }
        self.workspace
            .doc()
            .upsert_chat(&Chat {
                id: chat_id.to_string(),
                device_id: owner_device_id.to_string(),
                title: source.title.clone(),
                archived: false,
                cwd: Some(remote_cwd.clone()),
                branch: attached.environment.source_ref.clone(),
                checkout_id: None,
                config: Some(ChatConfig {
                    model: Some(remote_model),
                    agent_account_id: None,
                    sandbox: SandboxLevel::WorkspaceWrite,
                    ..config.clone()
                }),
                last_message_preview: None,
                last_message_at: None,
                created_at: chrono::Utc::now(),
                harness_session_id: Some(native_id.clone()),
                harness_session_cwd: Some(remote_cwd.clone()),
                fork_from: None,
                space_id: source.space_id.clone(),
                last_seen_at: None,
            })
            .map_err(|error| {
                recovery_error(&format!(
                    "native context imported but its local identity could not be persisted: {error}"
                ))
            })
    }

    fn admit_scaffold_handoff(
        &self,
        source: Chat,
        prompt: String,
        remote_model: String,
        actor_subject: String,
        attached: ScaffoldEnvironmentControlResult,
    ) -> Result<HandoffSessionToScaffoldResult, RpcError> {
        let chat_id = attached
            .environment
            .scope
            .session_id
            .as_ref()
            .ok_or_else(|| RpcError::Failed("native_handoff_target_identity_missing".into()))?
            .clone();
        self.require_no_handoff_command(&chat_id)?;
        let mut outcome = PreparationOutcome::for_command(
            self,
            &chat_id,
            attached.preparation_generation.as_deref(),
        )?;
        let config = source
            .config
            .as_ref()
            .ok_or_else(|| RpcError::Failed("native_handoff_source_config_missing".into()))?;
        let SessionEnvironmentSource::Scaffold { sandbox_id, .. } = &attached.environment.source
        else {
            return Err(RpcError::Failed(
                "native_handoff_environment_missing".into(),
            ));
        };
        validate_attachment(&attached, sandbox_id, &attached.environment.scope)?;
        self.install_scaffold_control_grant(&attached)?;
        let owner_device_id = attached
            .attached_device_id
            .as_ref()
            .expect("validated attachment device");
        let grant_id = attached
            .control_grant
            .as_ref()
            .expect("validated control grant")
            .id
            .clone();
        let remote_cwd = attached
            .handoff_cwd
            .as_ref()
            .filter(|cwd| !cwd.trim().is_empty())
            .ok_or_else(|| RpcError::Failed("native_handoff_remote_cwd_missing".into()))?
            .clone();
        let native_id = attached
            .handoff_native_session_id
            .as_ref()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| RpcError::Failed("native_handoff_native_identity_missing".into()))?
            .clone();
        self.persist_handoff_context(&source, remote_model.clone(), owner_device_id, &attached)?;
        let run = RunRequest {
            prompt,
            model: Some(remote_model),
            agent_account_id: None,
            reasoning: config.reasoning,
            model_options: config.model_options.clone(),
            cwd: remote_cwd.clone(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            resume: Some(native_id.clone()),
            attachments: Vec::new(),
        };
        let command_id = self
            .doc_host
            .queue_command(
                &chat_id,
                SessionCommandPayload::Control {
                    session_id: chat_id.clone(),
                    owner_device_id: owner_device_id.clone(),
                    actor_device_id: self.doc_host.device_id().to_string(),
                    actor_subject,
                    grant_id,
                    source: AgentSessionSource::Scaffold,
                    action: Box::new(comet_doc::SessionControlAction::Start {
                        request: run,
                        message_id: crate::new_id(),
                    }),
                },
            )
            .map_err(|error| {
                RpcError::Failed(format!(
                    "{error}; sandbox {sandbox_id}; session {chat_id}; initial command not admitted"
                ))
            })?;
        if let Some(outcome) = outcome.as_mut() {
            outcome.admitted(&command_id)?;
        }
        Ok(HandoffSessionToScaffoldResult {
            chat_id,
            sandbox_id: sandbox_id.clone(),
            command_id,
            environment: attached.environment,
        })
    }
}

pub(super) struct PreparationOutcome {
    workspace: WorkspaceHost,
    session_id: String,
    startup: comet_proto::SessionStartup,
    pub(super) armed: bool,
}

impl PreparationOutcome {
    pub(super) fn admitted(&mut self, command_id: &str) -> Result<(), RpcError> {
        // Admission already happened. Never downgrade it if metadata persistence fails.
        self.armed = false;
        self.startup.status = comet_proto::SessionStartupStatus::Admitted;
        self.startup.updated_at = chrono::Utc::now();
        self.startup.command_id = Some(command_id.to_string());
        self.workspace
            .update_session_startup(
                &self.session_id,
                Some(&self.startup.generation),
                self.startup.clone(),
            )
            .map_err(|error| {
                RpcError::Failed(format!("{error}; command {command_id} was admitted"))
            })
    }
    pub(super) fn for_command(
        rpc: &EngineRpc,
        session_id: &str,
        generation: Option<&str>,
    ) -> Result<Option<Self>, RpcError> {
        let startup = rpc
            .workspace
            .session_startup(session_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let Some(startup) = startup else {
            return if generation.is_some() {
                Err(RpcError::Failed(
                    "Scaffold preparation is no longer tracked".into(),
                ))
            } else {
                Ok(None)
            };
        };
        if generation.is_some_and(|generation| generation != startup.generation)
            || (generation.is_none()
                && startup.status != comet_proto::SessionStartupStatus::Admitted)
        {
            return Err(RpcError::Failed(
                "Scaffold preparation generation mismatch".into(),
            ));
        }
        if startup.status == comet_proto::SessionStartupStatus::Admitted {
            return Ok(None);
        }
        if startup.status == comet_proto::SessionStartupStatus::CreationUncertain {
            return Err(RpcError::Failed(
                "Scaffold creation outcome is unknown".into(),
            ));
        }
        Ok(Some(Self {
            workspace: rpc.workspace.clone(),
            session_id: session_id.to_string(),
            startup,
            armed: true,
        }))
    }
    fn begin(workspace: WorkspaceHost, session_id: &str) -> Result<Self, RpcError> {
        let startup = comet_proto::SessionStartup {
            generation: crate::new_id(),
            status: comet_proto::SessionStartupStatus::Preparing,
            updated_at: chrono::Utc::now(),
            command_id: None,
        };
        workspace
            .update_session_startup(session_id, None, startup.clone())
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        Ok(Self {
            workspace,
            session_id: session_id.to_string(),
            startup,
            armed: true,
        })
    }
}

impl Drop for PreparationOutcome {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = self
            .workspace
            .report_scaffold_preparation_failure(&self.session_id, &self.startup.generation)
        {
            tracing::warn!(session_id = %self.session_id, %error, "could not retain interrupted preparation outcome");
        }
    }
}

fn recovery_error(reason: &str) -> RpcError {
    RpcError::Failed(format!(
        "native handoff recovery refused: {reason}; check --recover-chat-id and --recover-sandbox-id against the preserved Crew target; no replacement will be created"
    ))
}

fn validate_recovery_environment(
    environment: &comet_proto::SessionEnvironment,
    sandbox_id: &str,
    scope: &CollaborationScope,
    database: ScaffoldDatabaseEnvironment,
) -> Result<(), RpcError> {
    if environment.scope != *scope
        || !matches!(&environment.source,
        SessionEnvironmentSource::Scaffold { sandbox_id: actual, .. } if actual == sandbox_id)
    {
        return Err(recovery_error(
            "accepted target sandbox/project/deployment/session scope mismatch",
        ));
    }
    if environment.database_environment != Some(database) {
        return Err(recovery_error(
            "database environment differs from the accepted target; pass its original --database-environment",
        ));
    }
    if matches!(
        &environment.source,
        SessionEnvironmentSource::Scaffold {
            lifecycle: ScaffoldLifecycle::AgentRunning,
            ..
        }
    ) {
        return Err(recovery_error("target is already running an agent"));
    }
    Ok(())
}

fn scaffold_control_error(error: crate::scaffold::ScaffoldError) -> RpcError {
    match error {
        crate::scaffold::ScaffoldError::AuthUnavailable => RpcError::ScaffoldAuthUnavailable,
        crate::scaffold::ScaffoldError::Transport { cause, .. } => {
            RpcError::Failed(format!("scaffold_request_failed: {cause}"))
        }
        error => RpcError::Failed(error.to_string()),
    }
}

fn checked_lifecycle(
    result: &ScaffoldEnvironmentControlResult,
    sandbox_id: &str,
    scope: &CollaborationScope,
) -> Result<ScaffoldLifecycle, RpcError> {
    let SessionEnvironmentSource::Scaffold {
        sandbox_id: actual,
        lifecycle,
        ..
    } = &result.environment.source
    else {
        return Err(RpcError::Failed(
            "Scaffold returned a local environment".into(),
        ));
    };
    if actual != sandbox_id || result.environment.scope != *scope {
        return Err(RpcError::Failed(
            "Scaffold returned a different sandbox scope".into(),
        ));
    }
    Ok(*lifecycle)
}

fn validate_attachment(
    result: &ScaffoldEnvironmentControlResult,
    sandbox_id: &str,
    scope: &CollaborationScope,
) -> Result<(), RpcError> {
    let lifecycle = checked_lifecycle(result, sandbox_id, scope)?;
    if matches!(
        lifecycle,
        ScaffoldLifecycle::Paused | ScaffoldLifecycle::Stopped | ScaffoldLifecycle::Failed
    ) {
        return Err(RpcError::Failed(format!(
            "Scaffold entered terminal lifecycle {lifecycle:?}"
        )));
    }
    let projection = result
        .room_projection
        .as_ref()
        .ok_or_else(|| RpcError::Failed("Scaffold attach returned no session room".into()))?;
    if projection.project_id != scope.project_id
        || Some(projection.deployment_id.as_str()) != scope.deployment_id.as_deref()
        || Some(projection.session_id.as_str()) != scope.session_id.as_deref()
    {
        return Err(RpcError::Failed(
            "Scaffold attach returned a different session room".into(),
        ));
    }
    let SessionEnvironmentSource::Scaffold {
        lifecycle_epoch, ..
    } = &result.environment.source
    else {
        unreachable!()
    };
    if !result
        .attached_device_id
        .as_deref()
        .and_then(comet_proto::parse_scaffold_device_id)
        .is_some_and(|(id, epoch)| id == sandbox_id && Some(epoch) == *lifecycle_epoch)
    {
        return Err(RpcError::Failed(
            "Scaffold attach returned a different device".into(),
        ));
    }
    if !result.control_grant.as_ref().is_some_and(|grant| {
        !grant.id.is_empty()
            && grant.expires_at > crate::now_ms()
            && grant
                .capabilities
                .iter()
                .any(|cap| cap == comet_proto::CAPABILITY_SESSION_CHAT)
    }) {
        return Err(RpcError::Failed(
            "Scaffold attach returned no chat authority".into(),
        ));
    }
    Ok(())
}

async fn prepare_scaffold_session_with<Control, ControlFuture>(
    params: PrepareScaffoldSessionParams,
    accepted: Option<comet_proto::SessionEnvironment>,
    mut control: Control,
) -> Result<ScaffoldEnvironmentControlResult, RpcError>
where
    Control: FnMut(ScaffoldEnvironmentControl) -> ControlFuture,
    ControlFuture: Future<Output = Result<ScaffoldEnvironmentControlResult, RpcError>>,
{
    let target_session_id = params.scope.session_id.clone().unwrap();
    // Creation is intentionally outside every retry loop. A lost create response
    // is not authority to allocate another sandbox.
    let mut sandbox_id = None;
    let prepare = async {
        let created = match accepted {
            Some(environment) => environment,
            None => {
                control(ScaffoldEnvironmentControl::Create {
                    scope: params.scope.clone(),
                    name: params.name,
                    source_ref: params.source_ref,
                    region: None,
                    database_environment: params.database_environment,
                    agent_route: params.agent_route,
                })
                .await?
                .environment
            }
        };
        let SessionEnvironmentSource::Scaffold {
            sandbox_id: created_id,
            ..
        } = &created.source
        else {
            return Err(RpcError::Failed(
                "Scaffold returned a local environment".into(),
            ));
        };
        sandbox_id = Some(created_id.clone());
        let scope = created.scope;
        if scope != params.scope {
            return Err(RpcError::Failed(
                "Scaffold creation returned a different session scope".into(),
            ));
        }
        // Attachment boots the remote Crew host: waiting for Ready first deadlocks.
        // Preserve the existing fault tolerance, but do not expire healthy startup.
        // This window only bounds consecutive retryable control failures.
        let mut fault_started = None;
        let mut attached = loop {
            match control(ScaffoldEnvironmentControl::Attach {
                sandbox_id: created_id.clone(),
                scope: scope.clone(),
            })
            .await
            {
                Ok(result) => break result,
                Err(error) if crate::scaffold::is_retryable_scaffold_control_error(&error) => {
                    let started = fault_started.get_or_insert_with(tokio::time::Instant::now);
                    if started.elapsed() >= TRANSIENT_FAULT_TIMEOUT {
                        return Err(error);
                    }
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                Err(error) => return Err(error),
            }
        };
        validate_attachment(&attached, created_id, &scope)?;
        fault_started = None;
        loop {
            let inspected = control(ScaffoldEnvironmentControl::Inspect {
                sandbox_id: created_id.clone(),
                scope: scope.clone(),
            })
            .await;
            match inspected {
                Ok(result) => {
                    let lifecycle = checked_lifecycle(&result, created_id, &scope)?;
                    let SessionEnvironmentSource::Scaffold {
                        lifecycle_epoch, ..
                    } = &result.environment.source
                    else {
                        unreachable!()
                    };
                    let SessionEnvironmentSource::Scaffold {
                        lifecycle_epoch: attached_epoch,
                        ..
                    } = &attached.environment.source
                    else {
                        unreachable!()
                    };
                    if lifecycle_epoch != attached_epoch
                        || result.environment.owner_principal
                            != attached.environment.owner_principal
                    {
                        return Err(RpcError::Failed(
                            "Scaffold lifecycle authority changed while waiting for readiness"
                                .into(),
                        ));
                    }
                    match lifecycle {
                        ScaffoldLifecycle::Ready | ScaffoldLifecycle::AgentRunning => {
                            attached.environment = result.environment;
                            break;
                        }
                        ScaffoldLifecycle::Paused
                        | ScaffoldLifecycle::Stopped
                        | ScaffoldLifecycle::Failed => {
                            return Err(RpcError::Failed(format!(
                                "Scaffold entered terminal lifecycle {lifecycle:?}"
                            )));
                        }
                        ScaffoldLifecycle::Creating
                        | ScaffoldLifecycle::RestoringSnapshot
                        | ScaffoldLifecycle::Starting
                        | ScaffoldLifecycle::Resuming => {}
                    }
                    fault_started = None;
                }
                Err(error) if crate::scaffold::is_retryable_scaffold_control_error(&error) => {
                    let started = fault_started.get_or_insert_with(tokio::time::Instant::now);
                    if started.elapsed() >= TRANSIENT_FAULT_TIMEOUT {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
            tokio::time::sleep(RETRY_DELAY).await;
        }
        validate_attachment(&attached, created_id, &scope)?;
        if let Some(handoff) = params.omp_handoff {
            let transferred = control(ScaffoldEnvironmentControl::HandoffOmpSession {
                sandbox_id: created_id.clone(),
                scope: scope.clone(),
                native_session_id: handoff.native_session_id,
                cwd: handoff.cwd,
            })
            .await?;
            attached.environment = transferred.environment;
            validate_attachment(&attached, created_id, &scope)?;
            if transferred
                .handoff_native_session_id
                .as_deref()
                .is_none_or(|id| id.trim().is_empty())
                || transferred
                    .handoff_cwd
                    .as_deref()
                    .is_none_or(|cwd| cwd.trim().is_empty())
            {
                return Err(RpcError::Failed(
                    "OMP handoff returned no native session identity".into(),
                ));
            }
            attached.handoff_native_session_id = transferred.handoff_native_session_id;
            attached.handoff_cwd = transferred.handoff_cwd;
        }
        Ok(attached)
    };
    let result = prepare.await;
    result.map_err(|error| {
            if let Some(sandbox_id) = sandbox_id {
                RpcError::Failed(format!("{error}; sandbox {sandbox_id}; session {target_session_id}; do not retry creation blindly"))
            } else {
                error
            }
        })
}

#[cfg(test)]
#[path = "scaffold_session_tests.rs"]
mod tests;
