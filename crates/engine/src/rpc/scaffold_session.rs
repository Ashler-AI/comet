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
        // Runtime clones share this gate. Reject a concurrent same-session call
        // before reading the accepted ref or issuing any create request.
        let _preparation = self.scaffold()?.preparation_gate(&params.scope)
            .try_lock_owned().map_err(|_| RpcError::Failed(
                "Scaffold session preparation already in progress; wait for the existing request".into()
            ))?;
        params.agent_route.validate().map_err(|error| RpcError::Failed(error.into()))?;
        let reference = self
            .workspace
            .doc()
            .session_ref(&actor.id, session_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        if reference.as_ref().is_some_and(|reference| {
            reference.environment.is_none() && reference.startup.as_ref().is_some_and(|startup|
                startup.status == comet_proto::SessionStartupStatus::CreationUncertain)
        }) {
            return Err(RpcError::Failed(
                "Sandbox creation outcome is unknown; inspect the existing request before retrying creation".into(),
            ));
        }
        let accepted = reference.and_then(|reference| reference.environment).filter(|environment| {
                matches!(
                    &environment.source,
                    SessionEnvironmentSource::Scaffold { .. }
                ) && environment.scope == params.scope
            });
        let mut startup = PreparationOutcome::begin(self.workspace.clone(), session_id)?;
        let expected_scope = params.scope.clone();
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let startup_state = &startup.startup;
        let mut result = prepare_scaffold_session_with(params, accepted, |control| {
            let expected_scope = &expected_scope;
            let cancellation = &cancellation;
            async move {
                let creating = matches!(&control, ScaffoldEnvironmentControl::Create { .. });
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
        let chat_id = crate::new_id();
        let scope = CollaborationScope {
            project_id: project_id.clone(),
            deployment_id: Some(self.scaffold()?.deployment_id().to_string()),
            session_id: Some(chat_id.clone()),
            unknown: Default::default(),
        };
        let remote_model = agent_route.omp_model();
        let attached = self
            .prepare_scaffold_session(PrepareScaffoldSessionParams {
                scope,
                name: source.title.clone(),
                source_ref: Some("master".into()),
                database_environment: params.database_environment,
                agent_route,
                omp_handoff: Some(OmpHandoff {
                    native_session_id: native_session_id.clone(),
                    cwd: cwd.clone(),
                }),
            })
            .await?;
        self.admit_scaffold_handoff(source, params.prompt, remote_model, actor_subject, attached)
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
        let mut outcome = PreparationOutcome::for_command(
            self, &chat_id, attached.preparation_generation.as_deref(),
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
        let remote_config = ChatConfig {
            model: Some(remote_model.clone()),
            agent_account_id: None,
            sandbox: SandboxLevel::WorkspaceWrite,
            ..config.clone()
        };
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
        let remote_chat = Chat {
            id: chat_id.clone(),
            device_id: source.device_id.clone(),
            title: source.title,
            archived: false,
            cwd: Some(remote_cwd.clone()),
            branch: attached.environment.source_ref.clone(),
            checkout_id: None,
            config: Some(remote_config),
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::Utc::now(),
            harness_session_id: Some(native_id),
            harness_session_cwd: Some(remote_cwd),
            fork_from: None,
            space_id: source.space_id,
            last_seen_at: None,
        };
        self.workspace
            .doc()
            .upsert_chat(&remote_chat)
            .map_err(|error| {
                RpcError::Failed(format!("{error}; sandbox {sandbox_id}; session {chat_id}"))
            })?;
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
        if let Some(outcome) = outcome.as_mut() { outcome.admitted(&command_id)?; }
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
        self.workspace.update_session_startup(
            &self.session_id, Some(&self.startup.generation), self.startup.clone(),
        ).map_err(|error| RpcError::Failed(format!("{error}; command {command_id} was admitted")))
    }
    pub(super) fn for_command(
        rpc: &EngineRpc,
        session_id: &str,
        generation: Option<&str>,
    ) -> Result<Option<Self>, RpcError> {
        let startup = rpc.workspace.session_startup(session_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let Some(startup) = startup else {
            return if generation.is_some() {
                Err(RpcError::Failed("Scaffold preparation is no longer tracked".into()))
            } else {
                Ok(None)
            };
        };
        if generation.is_some_and(|generation| generation != startup.generation)
            || (generation.is_none() && startup.status != comet_proto::SessionStartupStatus::Admitted)
        {
            return Err(RpcError::Failed("Scaffold preparation generation mismatch".into()));
        }
        if startup.status == comet_proto::SessionStartupStatus::Admitted {
            return Ok(None);
        }
        if startup.status == comet_proto::SessionStartupStatus::CreationUncertain {
            return Err(RpcError::Failed("Scaffold creation outcome is unknown".into()));
        }
        Ok(Some(Self {
            workspace: rpc.workspace.clone(), session_id: session_id.to_string(), startup, armed: true,
        }))
    }
    fn begin(workspace: WorkspaceHost, session_id: &str) -> Result<Self, RpcError> {
        let startup = comet_proto::SessionStartup {
            generation: crate::new_id(),
            status: comet_proto::SessionStartupStatus::Preparing,
            updated_at: chrono::Utc::now(),
            command_id: None,
        };
        workspace.update_session_startup(session_id, None, startup.clone())
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        Ok(Self { workspace, session_id: session_id.to_string(), startup, armed: true })
    }
}

impl Drop for PreparationOutcome {
    fn drop(&mut self) {
        if !self.armed { return; }
        if let Err(error) = self.workspace.report_scaffold_preparation_failure(
            &self.session_id, &self.startup.generation,
        ) {
            tracing::warn!(session_id = %self.session_id, %error, "could not retain interrupted preparation outcome");
        }
    }
}

fn scaffold_control_error(error: crate::scaffold::ScaffoldError) -> RpcError {
    match error {
        crate::scaffold::ScaffoldError::AuthUnavailable => RpcError::ScaffoldAuthUnavailable,
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
                    let SessionEnvironmentSource::Scaffold { lifecycle_epoch, .. } =
                        &result.environment.source
                    else {
                        unreachable!()
                    };
                    let SessionEnvironmentSource::Scaffold { lifecycle_epoch: attached_epoch, .. } =
                        &attached.environment.source
                    else {
                        unreachable!()
                    };
                    if lifecycle_epoch != attached_epoch
                        || result.environment.owner_principal != attached.environment.owner_principal
                    {
                        return Err(RpcError::Failed(
                            "Scaffold lifecycle authority changed while waiting for readiness".into(),
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
