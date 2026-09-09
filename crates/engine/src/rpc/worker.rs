//! Thin local orchestration over workspace rows, Repos, DocHost and SessionsEngine.
//! No inference, auth routing, task policy or filesystem cleanup lives here.
use super::*;
use comet_proto::{AgentEvent, DoneStatus, SandboxLevel, WorkerBinding};
use comet_rpc::{ControlWorkerSessionParams, EnsureWorkerSessionParams, WorkerSessionAction, WorkerSessionParams};
use std::path::Path;

fn failed(error: impl std::fmt::Display) -> RpcError {
    RpcError::Failed(error.to_string())
}

fn exact_id(value: &str) -> Result<(), RpcError> {
    if canonical_session_id(value).as_deref() != Some(value) {
        return Err(failed("worker_session_id_must_be_canonical_uuid"));
    }
    Ok(())
}

impl EngineRpc {
    fn worker_owner(&self, chat_id: &str, owner_chat_id: &str) -> Result<(), RpcError> {
        if self.runtime_profile != RuntimeProfile::LocalController {
            return Err(failed("worker_sessions_require_local_controller"));
        }
        exact_id(chat_id)?;
        exact_id(owner_chat_id)?;
        if chat_id == owner_chat_id {
            return Err(failed("worker_cannot_own_itself"));
        }
        let owner = self.workspace.doc().chat(owner_chat_id).map_err(failed)?
            .ok_or_else(|| failed("worker_owner_not_found"))?;
        if owner.device_id != self.doc_host.device_id() {
            return Err(failed("worker_owner_not_local"));
        }
        Ok(())
    }

    fn owned_worker(&self, p: &WorkerSessionParams) -> Result<WorkerBinding, RpcError> {
        self.worker_owner(&p.chat_id, &p.owner_chat_id)?;
        let binding = self.workspace.doc().worker_binding(&p.chat_id).map_err(failed)?
            .ok_or_else(|| failed("worker_not_found"))?;
        if binding.owner_chat_id != p.owner_chat_id || binding.owner_device_id != self.doc_host.device_id() {
            return Err(failed("worker_owner_mismatch"));
        }
        if let Some(chat) = self.workspace.doc().chat(&p.chat_id).map_err(failed)?
            && chat.device_id != binding.owner_device_id
        {
            return Err(failed("worker_host_changed"));
        }
        Ok(binding)
    }

    pub(super) async fn ensure_worker_session(&self, p: EnsureWorkerSessionParams) -> Result<serde_json::Value, RpcError> {
        self.worker_owner(&p.chat_id, &p.owner_chat_id)?;
        if !Path::new(&p.project_path).is_absolute() || p.base_ref.trim().is_empty()
            || p.title.trim().is_empty() || p.model.trim().is_empty()
        {
            return Err(failed("worker_requires_project_base_title_model_effort"));
        }
        let _operation = self.workspace.worker_operation().await;
        let project = self.repos.checkout_identity(Path::new(&p.project_path)).await.map_err(failed)?;
        if project.root != Path::new(&p.project_path) {
            return Err(failed("worker_project_must_be_canonical_checkout_root"));
        }
        let config = ChatConfig {
            harness: HarnessId::Omp,
            model: Some(p.model.clone()),
            reasoning: Some(p.effort),
            agent_account_id: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
        };
        let mut binding = if let Some(binding) = self.workspace.doc().worker_binding(&p.chat_id).map_err(failed)? {
            if binding.owner_chat_id != p.owner_chat_id || binding.owner_device_id != self.doc_host.device_id()
                || binding.project_path != p.project_path || binding.base_ref != p.base_ref
                || binding.title != p.title || binding.config != config
            {
                return Err(failed("worker_identity_conflict"));
            }
            if binding.closed || binding.paused {
                return Err(failed("worker_stopped_use_recover"));
            }
            binding
        } else {
            if self.workspace.doc().chat(&p.chat_id).map_err(failed)?.is_some() {
                return Err(failed("worker_chat_id_in_use"));
            }
            // Catalog validation only. It neither proves nor acquires credentials/quota.
            let harness = self.registry.resolve(HarnessId::Omp).map_err(failed)?;
            let models = harness.models().await.map_err(failed)?;
            let model = models.iter().find(|model| model.id == p.model)
                .ok_or_else(|| failed("worker_model_unavailable"))?;
            let efforts = if model.reasoning_levels.is_empty() { harness.reasoning_levels() } else { &model.reasoning_levels };
            if !efforts.contains(&p.effort) {
                return Err(failed("worker_effort_unsupported"));
            }
            let binding = WorkerBinding {
                chat_id: p.chat_id.clone(),
                owner_chat_id: p.owner_chat_id.clone(),
                owner_device_id: self.doc_host.device_id().to_string(),
                project_path: p.project_path.clone(),
                base_ref: p.base_ref.clone(),
                title: p.title.clone(),
                config: config.clone(),
                worktree: None,
                closed: false,
                paused: false,
            };
            self.workspace.doc().set_worker_binding(&binding).map_err(failed)?;
            self.workspace.persist().map_err(failed)?;
            binding
        };
        if binding.worktree.is_some() {
            self.verify_worker_checkout(&binding).await?;
            self.workspace.worker_ready(&p.chat_id).map_err(failed)?;
            self.workspace.persist().map_err(failed)?;
            return self.worker_snapshot(binding);
        }
        // Spaces use the same (device,path) identity and dedup rule as Mutate createSpace.
        let space_id = match self.workspace.read_spaces().map_err(failed)?.into_iter()
            .find(|space| space.device_id == binding.owner_device_id && space.path == p.project_path)
        {
            Some(space) => space.id,
            None => {
                let id = crate::new_id();
                self.workspace.create_space(&id, &binding.owner_device_id, &p.project_path, None, true).map_err(failed)?;
                id
            }
        };
        // Never expose the primary checkout as a runnable draft. The admission
        // fence remains closed until the isolated checkout is durably bound.
        self.workspace.create_chat(&p.chat_id, &space_id, Some(config), Some(String::new())).map_err(failed)?;
        self.workspace.rename_chat(&p.chat_id, &p.title).map_err(failed)?;
        self.workspace.persist().map_err(failed)?;
        let worktree = self.repos.ensure_worker_worktree(Path::new(&p.project_path), &p.base_ref, &p.chat_id).await.map_err(failed)?;
        if !self.workspace.bind_chat_worktree(&p.chat_id, &worktree).map_err(failed)? {
            // User cancellation wins. The retained binding/path licenses only
            // inspection, not recreation under a different id or checkout deletion.
            return Err(failed("worker_chat_missing_worktree_retained"));
        }
        binding.worktree = Some(worktree);
        self.workspace.doc().set_worker_binding(&binding).map_err(failed)?;
        self.workspace.persist().map_err(failed)?;
        self.worker_snapshot(binding)
    }

    async fn verify_worker_checkout(&self, binding: &WorkerBinding) -> Result<(), RpcError> {
        let Some(worktree) = &binding.worktree else { return Ok(()); };
        let identity = self.repos.checkout_identity(Path::new(&worktree.path)).await.map_err(failed)?;
        let member = self.repos.workspace_checkout(Path::new(&binding.project_path), Path::new(&worktree.path)).await;
        if identity.root != Path::new(&worktree.path) || worktree.checkout_id.as_deref() != Some(identity.id.as_str())
            || member.as_deref() != Some(Path::new(&worktree.path)) || identity.root == Path::new(&binding.project_path)
        {
            return Err(failed("worker_checkout_identity_changed"));
        }
        Ok(())
    }

    pub(super) async fn read_worker_session(&self, p: WorkerSessionParams) -> Result<serde_json::Value, RpcError> {
        let binding = self.owned_worker(&p)?;
        if !binding.closed && !binding.paused && binding.worktree.is_some()
            && self.workspace.doc().chat(&p.chat_id).map_err(failed)?.is_some()
        {
            self.workspace.worker_ready(&p.chat_id).map_err(failed)?;
        }
        self.verify_worker_checkout(&binding).await?;
        self.worker_snapshot(binding)
    }

    fn worker_snapshot(&self, binding: WorkerBinding) -> Result<serde_json::Value, RpcError> {
        let chat = self.workspace.doc().chat(&binding.chat_id).map_err(failed)?;
        let commands = match &chat {
            Some(_) => self.doc_host.open(&binding.chat_id).map_err(failed)?.doc().read_commands().map_err(failed)?,
            None => Vec::new(),
        };
        let replies = self.doc_host.open(&binding.owner_chat_id).map_err(failed)?.doc().read_commands().map_err(failed)?
            .into_iter().filter(|command| matches!(&command.payload,
                SessionCommandPayload::PeerMessage { source_chat_id, reply_to: Some(_), .. } if source_chat_id == &binding.chat_id))
            .collect::<Vec<_>>();
        // The ledger marks processed before side effects. A crash in that gap
        // cannot be called queued or successful, and must not be auto-replayed.
        let mut unresolved_processed = false;
        for command in &commands {
            if command.status == comet_doc::SessionCommandStatus::Pending
                && self.doc_host.command_processed(&command.id).map_err(failed)?
            {
                unresolved_processed = true;
            }
        }
        let latest = self.sessions.latest_event(&binding.chat_id).map_err(failed)?;
        let active = self.sessions.worker_active(&binding.chat_id);
        let status = self.sessions.session_status(&binding.chat_id);
        let state = if active {
            if status.as_ref().is_some_and(|status| status.status == SessionStatus::AwaitingInput) { "waiting" } else { "busy" }
        } else if binding.closed { "closed" }
        else if binding.paused { "interrupted" }
        else if chat.is_none() { "missing" }
        else if binding.worktree.is_none() { "provisioning" }
        else if unresolved_processed { "unknown" }
        else if commands.iter().any(|command| command.status == comet_doc::SessionCommandStatus::Pending) { "queued" }
        else if commands.last().is_some_and(|command| matches!(command.status,
            comet_doc::SessionCommandStatus::Rejected | comet_doc::SessionCommandStatus::Expired)) { "failed" }
        else { match latest.as_ref().map(|(_, event)| event) {
            Some(AgentEvent::Done { status: DoneStatus::Completed, .. }) => "completed",
            Some(AgentEvent::Done { status: DoneStatus::Errored, .. }) => "failed",
            Some(_) => "interrupted",
            None => "idle",
        }};
        let latest_event = latest.map(|(seq, event)| serde_json::json!({"seq":seq,"event":event}));
        Ok(serde_json::json!({
            "version":1, "binding":binding, "chat":chat, "state":state,
            "latestEvent":latest_event, "commands":commands, "replies":replies
        }))
    }

    pub(super) async fn control_worker_session(&self, p: ControlWorkerSessionParams) -> Result<serde_json::Value, RpcError> {
        let _operation = self.workspace.worker_operation().await;
        let identity = WorkerSessionParams { chat_id: p.chat_id.clone(), owner_chat_id: p.owner_chat_id };
        let mut binding = self.owned_worker(&identity)?;
        // Interrupt and close remain usable if the checkout was moved/missing;
        // they never touch the filesystem. Recover must prove it still exists.
        if p.action == WorkerSessionAction::Recover {
            self.verify_worker_checkout(&binding).await?;
            if binding.worktree.is_none() { return Err(failed("worker_provisioning_retry_ensure")); }
            if self.workspace.doc().chat(&p.chat_id).map_err(failed)?.is_none() { return Err(failed("worker_chat_missing")); }
            if self.sessions.worker_active(&p.chat_id) { return Err(failed("worker_still_active")); }
            // Validate the full immutable binding before opening the fence.
            let chat = self.workspace.doc().chat(&p.chat_id).map_err(failed)?.ok_or_else(|| failed("worker_chat_missing"))?;
            let worktree = binding.worktree.as_ref().expect("checked above");
            if chat.cwd.as_deref() != Some(worktree.path.as_str())
                || chat.checkout_id != worktree.checkout_id || chat.config.as_ref() != Some(&binding.config)
            {
                return Err(failed("worker_binding_changed"));
            }
            if binding.paused || binding.closed {
                self.doc_host.cancel_worker_commands(&p.chat_id).map_err(failed)?;
            }
            binding.closed = false;
            binding.paused = false;
            self.workspace.set_chat_archived(&p.chat_id, false).map_err(failed)?;
            self.workspace.doc().set_worker_binding(&binding).map_err(failed)?;
        } else {
            binding.paused = true;
            binding.closed |= p.action == WorkerSessionAction::Close;
            self.workspace.doc().set_worker_binding(&binding).map_err(failed)?;
            self.workspace.persist().map_err(failed)?;
            self.sessions.quiesce_worker(&p.chat_id).await.map_err(failed)?;
            self.doc_host.cancel_worker_commands(&p.chat_id).map_err(failed)?;
            if binding.closed {
                // Never call Mutate setChatArchived: that may stage deletion.
                self.workspace.doc().remove_worktree_deletion(&p.chat_id).map_err(failed)?;
                self.workspace.set_chat_archived(&p.chat_id, true).map_err(failed)?;
            }
        }
        self.workspace.persist().map_err(failed)?;
        self.doc_host.persist_chat(&p.chat_id).map_err(failed)?;
        self.worker_snapshot(binding)
    }
}
