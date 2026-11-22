//! Crew: Scaffold sandboxes behind upstream's Cloud RPCs
//! (docs/crew/zgx-thin-port.md, "Scaffold sandboxes", piece 3), so zgx's own
//! Cloud row ("Work in") creates and attaches Crew's sandboxes with no new
//! Luau. A signed-in Scaffold desktop's engine answers:
//!
//! - `CloudStatus`: `{state: ready, available, deviceId: "cloud"}`, available
//!   only with a sandbox region (`crew-runtime.json` `sandboxRegion`): the
//!   Cloud row's agents and models are then OMP's own (`rpc.rs`
//!   `answer_for_cloud`);
//! - `ListGithubRepos`: Ashler-AI/ashler-platform alone; `ListCloudBranches`:
//!   the deployment's default;
//! - `CloudSessions`: this region's sandboxes as Platform lists them now;
//! - a Cloud chat (`cloud_chat_host`): its sandbox created in that region
//!   for the chat (its session id is the chat id), the chat's row published on
//!   the sandbox's host (`comet-scaffold-{sandbox}-e{epoch}`, no project,
//!   `/workspace/ashler-platform`, OMP), and the host attached in the
//!   background;
//! - wake, sleep, delete, sizes, reach and billing: refused, with a sentence.
//!
//! Create and Attach are durable commands ([`ledger`]): claimed before they
//! are sent, settled once. One whose outcome never came is reconciled by
//! observation only: a lost Create is looked for by its session id and never
//! sent again; a lost Attach is probed, never minted or launched again. A
//! 4xx is a rejection; anything else leaves the outcome unknown.
//!
//! A sandbox's chat row carries Crew's `crew` metadata, the binding the Edge
//! admits the host by (`edge/src/crew/registry-scope.ts`): the row's device
//! is the host and `crew.environment` names its sandbox and epoch. It's
//! written with no project: the host can't see this device's projects, and
//! the Edge refuses a host whose chat names another device's.
//!
//! [`guard_scaffold_target`] keeps a forwarded call from reaching a sandbox
//! host unless the Edge lets it through (one refused relay frame ends the
//! whole device link): Crew's `guard_scaffold_target`, on the Edge's list as
//! this branch has it.

#[path = "crew_ledger.rs"]
mod ledger;
#[path = "crew_scaffold.rs"]
pub(crate) mod scaffold;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeron_harness::CancellationToken;
use zeron_proto::{ChatConfig, CloudSession, CloudSessions, CloudState, CloudStatus, Device, GithubRepo, HarnessId, RepoRef};
use zeron_rpc::{RpcError, RpcReply, TokenSource, methods};

use crate::auth::{Auth, ScaffoldScope};
use crate::workspace_host::WorkspaceHost;
use ledger::{Ledger, LedgerError};
use scaffold::{
    AgentRoute, Attachment, CreateOptions, EdgeDeviceJoinGrantClient, Lifecycle, SandboxHosts, ScaffoldClient,
    ScaffoldError, SessionEnvironment, device_id_for, parse_device_id,
};

/// The agents a sandbox's chat runs: OMP, as Crew's sandboxes do.
pub(crate) const HARNESSES: &[HarnessId] = &[HarnessId::Omp];
/// The Cloud device the Cloud row is picked as (`CloudStatus.deviceId`).
const CLOUD_DEVICE: &str = zeron_proto::CLOUD_ACCOUNTS_DEVICE;
/// The deployment's default branch: what every sandbox starts from.
const DEFAULT_BRANCH: &str = "master";
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const RECONCILE_STEP: Duration = Duration::from_secs(30);
/// An attach waits for its sandbox's runtime as long as Platform says it's
/// starting; this bounds it all the same.
const ATTACH_DEADLINE: Duration = Duration::from_secs(10 * 60);

fn refused(sentence: impl Into<String>) -> RpcError {
    RpcError::Failed(sentence.into())
}

fn failed(error: impl std::fmt::Display) -> RpcError {
    RpcError::Failed(error.to_string())
}

/// A sandbox host's device (`comet-scaffold-{sandbox}-e{epoch}`).
pub(crate) fn is_sandbox_host(device_id: &str) -> bool {
    parse_device_id(device_id).is_some()
}

/// Crew's chat metadata (`zeron_proto::CrewChatMetadata`), the row's `crew`
/// field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    owner_subject: String,
    scope: ScaffoldScope,
    session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment: Option<SessionEnvironment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_route: Option<AgentRoute>,
}

/// A command's settled outcome in the ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Outcome {
    /// `confirmed` or `rejected`.
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment: Option<SessionEnvironment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attachment: Option<Attachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Who controls: the verified subject and email, in the verified project and
/// deployment.
#[derive(Debug, Clone)]
struct Controller {
    subject: String,
    email: String,
    project: String,
    deployment: String,
}

impl Controller {
    fn scope(&self, session: &str) -> ScaffoldScope {
        ScaffoldScope {
            project_id: self.project.clone(),
            deployment_id: Some(self.deployment.clone()),
            session_id: Some(session.to_owned()),
        }
    }
}

/// The Crew adapter behind the Cloud RPCs: a signed-in Scaffold desktop's.
pub struct CrewCloud {
    inner: Arc<Inner>,
}

/// Where the controller comes from: the engine's verified sign-in.
enum Identity {
    Auth(Auth),
    #[cfg(test)]
    Fixed(Controller),
}

struct Inner {
    identity: Identity,
    workspace: WorkspaceHost,
    hosts: SandboxHosts,
    /// `crew-runtime.json`'s `sandboxRegion`: the region whose Edge is this
    /// engine's. None: nothing is created.
    region: Option<String>,
    /// `crew-runtime.json`'s `deploymentId`, when it names one.
    deployment: Option<String>,
    ledger: Ledger,
    /// Claims are serialized (Crew's control gate).
    gate: tokio::sync::Mutex<()>,
    /// Attaches running in this process, by command id.
    attaching: Mutex<HashSet<String>>,
    stop: CancellationToken,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl CrewCloud {
    /// The adapter for a desktop on a Scaffold authority's synced profile;
    /// none for a sandbox host or without an authority.
    pub fn install(
        config: &crate::EngineConfig,
        auth: &Auth,
        tokens: Arc<dyn TokenSource>,
        workspace: WorkspaceHost,
        store_root: &Path,
    ) -> Option<Arc<Self>> {
        let scaffold = config.scaffold.as_ref()?;
        if auth.device_mode() || config.device.is_some() {
            return None;
        }
        let built = (|| -> Result<Self, String> {
            let client = ScaffoldClient::new(&scaffold.control_origin, &scaffold.project_scope, tokens.clone())
                .map_err(|error| error.to_string())?;
            let grants = EdgeDeviceJoinGrantClient::new(&config.edge_url, tokens).map_err(|error| error.to_string())?;
            let hosts = SandboxHosts::new(client, &config.edge_url, Arc::new(grants)).map_err(|error| error.to_string())?;
            let ledger = Ledger::open(store_root).map_err(|error| error.to_string())?;
            Ok(Self::from_parts(
                Identity::Auth(auth.clone()),
                workspace,
                hosts,
                scaffold.sandbox_region.clone(),
                scaffold.deployment_id.clone(),
                ledger,
            ))
        })();
        match built {
            Ok(cloud) => Some(Arc::new(cloud)),
            Err(error) => {
                tracing::warn!(%error, "crew: Scaffold sandboxes unavailable");
                None
            }
        }
    }

    fn from_parts(
        identity: Identity,
        workspace: WorkspaceHost,
        hosts: SandboxHosts,
        region: Option<String>,
        deployment: Option<String>,
        ledger: Ledger,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                identity,
                workspace,
                hosts,
                region,
                deployment,
                ledger,
                gate: tokio::sync::Mutex::new(()),
                attaching: Mutex::new(HashSet::new()),
                stop: CancellationToken::new(),
            }),
        }
    }

    /// Reconcile and refresh in the background, every 30 s once the registry
    /// has synced: observations only (no create, attach or prompt).
    pub fn start(self: &Arc<Self>) {
        let weak = Arc::downgrade(&self.inner);
        let stop = self.inner.stop.clone();
        tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else { return };
                let mut synced = inner.workspace.watch_registry_synced();
                drop(inner);
                tokio::select! {
                    _ = stop.cancelled() => return,
                    ready = synced.wait_for(|synced| *synced) => if ready.is_err() { return },
                }
                let Some(inner) = weak.upgrade() else { return };
                let cloud = Self { inner };
                if let Err(error) = cloud.reconcile_all().await {
                    tracing::debug!(%error, "crew: sandbox observation retained for later");
                }
                drop(cloud);
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(RECONCILE_INTERVAL) => {}
                }
            }
        });
    }

    pub fn shutdown(&self) {
        self.inner.stop.cancel();
    }

    fn controller(&self) -> Result<Controller, RpcError> {
        let auth = match &self.inner.identity {
            Identity::Auth(auth) => auth,
            #[cfg(test)]
            Identity::Fixed(controller) => return Ok(controller.clone()),
        };
        if auth.device_mode() {
            return Err(refused("A sandbox host doesn't control sandboxes."));
        }
        let session = auth
            .verified_scaffold_session()
            .ok_or_else(|| refused("Sign in to Crew to use its sandboxes."))?;
        let scope = auth.verified_scope().ok_or_else(|| refused("Sign in to Crew to use its sandboxes."))?;
        let deployment = scope
            .deployment_id
            .clone()
            .or_else(|| self.inner.deployment.clone())
            .ok_or_else(|| refused("Crew's sandboxes need a deployment (crew-runtime.json's deploymentId)."))?;
        if self.inner.deployment.as_ref().is_some_and(|configured| *configured != deployment) {
            return Err(refused("This Crew sign-in is for another deployment than crew-runtime.json names."));
        }
        Ok(Controller {
            subject: session.identity.user_id,
            email: session.identity.email,
            project: scope.project_id,
            deployment,
        })
    }

    fn status(&self) -> CloudStatus {
        CloudStatus {
            state: CloudState::Ready,
            device_id: Some(CLOUD_DEVICE.into()),
            available: self.inner.region.is_some() && self.controller().is_ok(),
            ..CloudStatus::default()
        }
    }

    fn repo() -> GithubRepo {
        GithubRepo {
            full_name: scaffold::HOST_REPO.into(),
            clone_url: format!("https://github.com/{}.git", scaffold::HOST_REPO),
            default_branch: DEFAULT_BRANCH.into(),
            private: true,
            ..GithubRepo::default()
        }
    }

    /// The Cloud RPCs a Crew engine answers itself; `None` for the rest
    /// (upstream's, unavailable with a Scaffold authority).
    pub(crate) async fn cloud_rpc(&self, method: &str, params: &Value) -> Option<Result<RpcReply, RpcError>> {
        let refusal = match method {
            methods::CLOUD_STATUS | methods::CLOUD_ENABLE => return Some(RpcReply::value(&self.status())),
            methods::CLOUD_SESSIONS => {
                return Some(match self.sessions().await {
                    Ok(sessions) => RpcReply::value(&sessions),
                    Err(error) => Err(error),
                });
            }
            methods::LIST_GITHUB_REPOS => return Some(RpcReply::value(&json!({ "repos": [Self::repo()] }))),
            methods::LIST_CLOUD_BRANCHES => {
                let repo = params.get("repo").and_then(Value::as_str).unwrap_or_default();
                if !repo.eq_ignore_ascii_case(scaffold::HOST_REPO) {
                    return Some(Err(refused(format!("Crew's sandboxes clone {} only.", scaffold::HOST_REPO))));
                }
                let default = RepoRef { name: DEFAULT_BRANCH.into(), current: true, worktree_path: None };
                return Some(RpcReply::value(&[default]));
            }
            methods::CLOUD_SESSION_WAKE => "Crew's sandboxes can't be woken from here yet.",
            // (A failed sandbox's alert offers it: the way on, said again.)
            methods::CLOUD_SESSION_RETRY => "Crew's sandboxes can't be restarted from here yet. Start a new chat to try again.",
            methods::CLOUD_SESSION_SLEEP => "Crew's sandboxes can't be put to sleep from here yet.",
            methods::CLOUD_SESSION_DELETE | methods::CLOUD_DELETE => "Crew's sandboxes can't be deleted from here yet.",
            methods::CLOUD_SET_MACHINE => "Crew's sandboxes have one size.",
            methods::CLOUD_SET_REACH | methods::CLOUD_SESSION_SET_REACH => "Crew's sandboxes reach only their own session.",
            methods::CLOUD_USAGE | methods::BILLING_STATUS | methods::BILLING_CHECKOUT | methods::BILLING_PORTAL => {
                "Crew's sandboxes have no credits or billing."
            }
            _ => return None,
        };
        Some(Err(refused(refusal)))
    }

    /// This region's sandboxes, as Platform lists them now, their chat rows
    /// refreshed (rows only).
    async fn refresh(&self, controller: &Controller) -> Result<Vec<SessionEnvironment>, RpcError> {
        let Some(region) = self.inner.region.as_deref() else { return Ok(Vec::new()) };
        let scope = ScaffoldScope {
            project_id: controller.project.clone(),
            deployment_id: Some(controller.deployment.clone()),
            session_id: None,
        };
        let listed = self
            .inner
            .hosts
            .client()
            .list(&scope, &self.inner.stop.child_token())
            .await
            .map_err(|error| refused(words(&error, "Crew couldn't list its sandboxes")))?;
        let ours: Vec<_> = listed
            .into_iter()
            .filter(|environment| environment.region() == Some(region) && self.verify(controller, environment).is_ok())
            .collect();
        for environment in &ours {
            if let Err(error) = self.publish(controller, environment, None, None) {
                tracing::debug!(%error, "crew: a sandbox's chat row wasn't refreshed");
            }
        }
        Ok(ours)
    }

    async fn sessions(&self) -> Result<CloudSessions, RpcError> {
        let controller = self.controller()?;
        let environments = self.refresh(&controller).await?;
        let sessions = environments
            .iter()
            .filter_map(|environment| {
                let (sandbox, lifecycle, epoch) = environment.sandbox().ok()?;
                Some(CloudSession {
                    chat_id: environment.scope.session_id.clone()?,
                    device_id: device_id_for(sandbox, epoch?),
                    space_id: String::new(),
                    repo: Some(scaffold::HOST_REPO.into()),
                    path: Some(scaffold::HOST_CWD.into()),
                    state: cloud_state(lifecycle),
                    error: (lifecycle == Lifecycle::Failed).then(|| failed_words(environment.failure.as_deref())),
                    created_at: environment.last_activity_at.unwrap_or_default(),
                    last_active_at: environment.last_activity_at,
                    ..CloudSession::default()
                })
            })
            .collect();
        Ok(CloudSessions { sessions, available: true })
    }

    /// `cloud_chat_host`, for a Cloud chat: its sandbox (created, or the one
    /// it already has), its row on the sandbox's host, and that host
    /// attached in the background. The host's device and the chat's folder.
    pub(crate) async fn chat_host(
        self: &Arc<Self>,
        chat_id: &str,
        branch: Option<&str>,
        config: Option<&ChatConfig>,
    ) -> Result<(String, String), RpcError> {
        if !zeron_rpc::caller().is_local() {
            return Err(refused("Only this computer's app starts Crew sandboxes."));
        }
        let controller = self.controller()?;
        let region = self
            .inner
            .region
            .clone()
            .ok_or_else(|| refused("No sandbox region is set for this Crew (crew-runtime.json's sandboxRegion)."))?;
        if branch.is_some_and(|branch| branch != DEFAULT_BRANCH) {
            return Err(refused(format!("Crew's sandboxes start from {DEFAULT_BRANCH}.")));
        }
        if uuid::Uuid::parse_str(chat_id).map(|id| id.hyphenated().to_string()).ok().as_deref() != Some(chat_id) {
            return Err(RpcError::BadParams("a sandbox's chat id is a lowercase UUID".into()));
        }
        if !self.inner.workspace.registry_synced() {
            return Err(refused("Crew is still syncing. Try again in a moment."));
        }
        if let Some(chat) = self.inner.workspace.chat(chat_id).map_err(failed)? {
            // Made already (a retried create): the same chat on the same host.
            if is_sandbox_host(&chat.device_id) {
                return Ok((chat.device_id, chat.cwd.unwrap_or_else(|| scaffold::HOST_CWD.into())));
            }
            return Err(refused("This chat already runs on another device."));
        }
        let route = config
            .and_then(|config| {
                let model = config.model.as_deref()?;
                let account = config.model_options.get("crewAccountId").and_then(Value::as_str);
                AgentRoute::for_chat(&config.harness, model, account)
            })
            .ok_or_else(|| refused("Pick one of OMP's models for the sandbox."))?;
        let scope = controller.scope(chat_id);
        let environment = self.create(&controller, &scope, &region, &route).await?;
        let (sandbox, _, epoch) = environment.sandbox().map_err(failed)?;
        let device = device_id_for(sandbox, epoch.ok_or_else(|| failed("The sandbox has no lifecycle epoch."))?);
        self.publish(&controller, &environment, Some(&route), None)?;
        self.spawn_attach(environment, Some(route));
        Ok((device, scaffold::HOST_CWD.into()))
    }

    /// Create `scope`'s sandbox, durably: claimed first; sent once; an
    /// unknown outcome observed (listed by its session), never sent again.
    async fn create(
        &self,
        controller: &Controller,
        scope: &ScaffoldScope,
        region: &str,
        route: &AgentRoute,
    ) -> Result<SessionEnvironment, RpcError> {
        let session = scope.session_id.as_deref().unwrap_or_default();
        let id = format!("create:{session}");
        let request = serde_json::to_vec(&json!({
            "principal": controller.subject, "scope": scope, "region": region, "agentRoute": route,
        }))
        .map_err(failed)?;
        let fresh = {
            let _gate = self.inner.gate.lock().await;
            self.inner.ledger.claim(&id, &request).map_err(|error| match error {
                LedgerError::Conflict(_) => refused("This chat's sandbox was asked for with another model or region."),
                error => failed(error),
            })?
        };
        if !fresh {
            return match self.settled(&id)? {
                Some(outcome) => outcome_result(outcome),
                None => match self.reconcile_create(&id, controller, scope).await? {
                    Some(environment) => Ok(environment),
                    None => Err(refused("This chat's sandbox isn't confirmed yet. Crew is looking for it and won't create another.")),
                },
            };
        }
        let options = CreateOptions { region: Some(region), ..CreateOptions::default() };
        let created = self
            .inner
            .hosts
            .client()
            .create(scope, options, route, &self.inner.stop.child_token())
            .await
            .and_then(|environment| self.verify(controller, &environment).map(|()| environment));
        match created {
            Ok(environment) => {
                self.finish(&id, Outcome::confirmed(Some(environment.clone()), None))?;
                Ok(environment)
            }
            Err(error) if error.is_rejection() => {
                let sentence = words(&error, "Crew couldn't start a sandbox");
                self.finish(&id, Outcome::rejected(&sentence))?;
                Err(refused(sentence))
            }
            Err(error) => {
                tracing::warn!(%error, command = %id, "crew: a sandbox create's outcome is unknown; it will be observed, never sent again");
                Err(refused("Crew couldn't confirm the sandbox. It will look for it and won't create another."))
            }
        }
    }

    /// A Create whose outcome never came: its session's sandbox, if Platform
    /// lists one in this region (settled then), else still unknown.
    async fn reconcile_create(&self, id: &str, controller: &Controller, scope: &ScaffoldScope) -> Result<Option<SessionEnvironment>, RpcError> {
        let region = self.inner.region.as_deref().ok_or_else(|| refused("No sandbox region is set."))?;
        let mut found = self
            .inner
            .hosts
            .client()
            .list(scope, &self.inner.stop.child_token())
            .await
            .map_err(failed)?
            .into_iter()
            .filter(|environment| environment.scope == *scope);
        let Some(environment) = found.next() else { return Ok(None) };
        if found.next().is_some() {
            return Err(refused("More than one sandbox claims this chat; nothing was changed."));
        }
        if environment.region() != Some(region) {
            self.finish(id, Outcome::rejected("The sandbox is in another region, whose Edge isn't this Crew's."))?;
            return Err(refused("The sandbox is in another region, whose Edge isn't this Crew's."));
        }
        if let Err(error) = self.verify(controller, &environment) {
            let sentence = words(&error, "The sandbox didn't check out");
            self.finish(id, Outcome::rejected(&sentence))?;
            return Err(refused(sentence));
        }
        self.finish(id, Outcome::confirmed(Some(environment.clone()), None))?;
        Ok(Some(environment))
    }

    /// Platform's answer is this controller's sandbox, in this region.
    fn verify(&self, controller: &Controller, environment: &SessionEnvironment) -> Result<(), ScaffoldError> {
        let session = environment.scope.session_id.as_deref().unwrap_or_default();
        if environment.scope != controller.scope(session) || !environment.owner_principal.eq_ignore_ascii_case(&controller.email) {
            return Err(ScaffoldError::InvalidScope("the sandbox belongs to another owner or deployment".into()));
        }
        if environment.region() != self.inner.region.as_deref() {
            return Err(ScaffoldError::InvalidScope("the sandbox is in another region, whose Edge isn't this Crew's".into()));
        }
        if environment.sandbox()?.2.is_none() {
            return Err(ScaffoldError::InvalidResponse("the sandbox has no lifecycle epoch".into()));
        }
        Ok(())
    }

    fn settled(&self, id: &str) -> Result<Option<Outcome>, RpcError> {
        let Some((_, outcome)) = self.inner.ledger.get(id).map_err(failed)? else { return Ok(None) };
        outcome.map(|bytes| serde_json::from_slice(&bytes).map_err(failed)).transpose()
    }

    fn finish(&self, id: &str, outcome: Outcome) -> Result<(), RpcError> {
        let bytes = serde_json::to_vec(&outcome).map_err(failed)?;
        self.inner.ledger.finish(id, &bytes).map_err(failed)
    }

    /// Attach `environment`'s host as a durable command, in the background.
    fn spawn_attach(self: &Arc<Self>, environment: SessionEnvironment, route: Option<AgentRoute>) {
        let Ok((sandbox, _, Some(epoch))) = environment.sandbox() else { return };
        let id = format!("attach:{sandbox}:e{epoch}");
        if !self.inner.attaching.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id.clone()) {
            return;
        }
        let cloud = self.clone();
        tokio::spawn(async move {
            if let Err(error) = cloud.attach(&id, &environment, route.as_ref()).await {
                tracing::warn!(%error, command = %id, "crew: the sandbox host wasn't attached");
            }
            cloud.inner.attaching.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
        });
    }

    async fn attach(&self, id: &str, environment: &SessionEnvironment, route: Option<&AgentRoute>) -> Result<(), RpcError> {
        let controller = self.controller()?;
        let (sandbox, _, epoch) = environment.sandbox().map_err(failed)?;
        let request = serde_json::to_vec(&json!({
            "principal": controller.subject, "scope": environment.scope, "sandboxId": sandbox, "lifecycleEpoch": epoch,
        }))
        .map_err(failed)?;
        let fresh = {
            let _gate = self.inner.gate.lock().await;
            self.inner.ledger.claim(id, &request).map_err(failed)?
        };
        if !fresh {
            // Sent before (perhaps): only observed now.
            if self.settled(id)?.is_none() {
                self.reconcile_attach(id, &controller, environment).await?;
            }
            return Ok(());
        }
        let cancellation = self.inner.stop.child_token();
        let attached = tokio::time::timeout(ATTACH_DEADLINE, self.inner.hosts.attach(sandbox, &environment.scope, &cancellation)).await;
        match attached {
            Ok(Ok((current, attachment))) => {
                self.finish(id, Outcome::confirmed(Some(current.clone()), Some(attachment.clone())))?;
                tracing::info!(command = %id, device = %attachment.device_id, run = ?attachment.run_id, "crew: the sandbox host is attached");
                self.publish(&controller, &current, route, Some(&attachment)).map(drop)
            }
            Ok(Err(error)) if error.is_rejection() => self.finish(id, Outcome::rejected(&error.to_string())),
            Ok(Err(error)) => Err(failed(format!("unknown outcome, to be observed: {error}"))),
            Err(_) => Err(failed("unknown outcome, to be observed: the attach outlived its deadline")),
        }
    }

    /// An Attach whose outcome never came: settled only by what its host
    /// answers to the probe; a sandbox gone terminal retires it.
    async fn reconcile_attach(&self, id: &str, controller: &Controller, environment: &SessionEnvironment) -> Result<(), RpcError> {
        let (sandbox, _, epoch) = environment.sandbox().map_err(failed)?;
        let cancellation = self.inner.stop.child_token();
        let current = self
            .inner
            .hosts
            .client()
            .inspect(sandbox, &environment.scope, &cancellation)
            .await
            .map_err(failed)?;
        let (_, lifecycle, current_epoch) = current.sandbox().map_err(failed)?;
        if lifecycle.terminal() || current_epoch != epoch {
            return self.finish(id, Outcome::rejected("Retired: the sandbox stopped or moved to another epoch; nothing was sent again."));
        }
        if let Some(attachment) = self.inner.hosts.observe(&current, &cancellation).await.map_err(failed)? {
            self.finish(id, Outcome::confirmed(Some(current.clone()), Some(attachment.clone())))?;
            self.publish(controller, &current, None, Some(&attachment))?;
        }
        Ok(())
    }

    /// Every command still unknown, observed; then this region's sandboxes,
    /// refreshed.
    async fn reconcile_all(&self) -> Result<(), RpcError> {
        let controller = self.controller()?;
        let mut after = String::new();
        loop {
            let batch = self.inner.ledger.pending(&after, 64).map_err(failed)?;
            if batch.is_empty() {
                break;
            }
            for (id, request) in batch {
                after.clone_from(&id);
                if self.inner.attaching.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&id) {
                    continue;
                }
                match tokio::time::timeout(RECONCILE_STEP, self.reconcile(&id, &request, &controller)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::debug!(command = %id, %error, "crew: still unconfirmed; nothing sent again"),
                    Err(_) => tracing::debug!(command = %id, "crew: observation timed out; nothing sent again"),
                }
            }
        }
        self.refresh(&controller).await.map(drop)
    }

    async fn reconcile(&self, id: &str, request: &[u8], controller: &Controller) -> Result<(), RpcError> {
        let request: Value = serde_json::from_slice(request).map_err(failed)?;
        if request["principal"].as_str() != Some(controller.subject.as_str()) {
            return Ok(());
        }
        let scope: ScaffoldScope = serde_json::from_value(request["scope"].clone()).map_err(failed)?;
        if id.starts_with("create:") {
            if let Some(environment) = self.reconcile_create(id, controller, &scope).await? {
                self.publish(controller, &environment, serde_json::from_value(request["agentRoute"].clone()).ok().as_ref(), None)?;
            }
            return Ok(());
        }
        let sandbox = request["sandboxId"].as_str().unwrap_or_default();
        let environment = self
            .inner
            .hosts
            .client()
            .inspect(sandbox, &scope, &self.inner.stop.child_token())
            .await
            .map_err(failed)?;
        if environment.sandbox().map_err(failed)?.2 != request["lifecycleEpoch"].as_u64() {
            return self.finish(id, Outcome::rejected("Retired: the sandbox moved to another epoch; nothing was sent again."));
        }
        self.reconcile_attach(id, controller, &environment).await
    }

    /// Publish `environment`'s chat (Crew's `publish_crew_environment`, less
    /// Start): on its host, room 2, no project, Crew's metadata naming its
    /// sandbox and epoch, the route's OMP config on a new row or a new
    /// route, and the host's device row if there's none. Never re-homes a
    /// chat of another device, never binds another sandbox, never goes back
    /// to an older epoch. Whether anything was written.
    fn publish(
        &self,
        controller: &Controller,
        environment: &SessionEnvironment,
        route: Option<&AgentRoute>,
        attachment: Option<&Attachment>,
    ) -> Result<bool, RpcError> {
        let workspace = &self.inner.workspace;
        if !workspace.registry_synced() {
            return Err(refused("The sandbox is durable; its chat waits for the registry to sync."));
        }
        let session = environment
            .scope
            .session_id
            .clone()
            .ok_or_else(|| failed("The sandbox has no session."))?;
        self.verify(controller, environment).map_err(failed)?;
        let (sandbox, lifecycle, epoch) = environment.sandbox().map_err(failed)?;
        let epoch = epoch.ok_or_else(|| failed("The sandbox has no lifecycle epoch."))?;
        let host = device_id_for(sandbox, epoch);
        if attachment.is_some_and(|attachment| attachment.device_id != host) {
            return Err(failed("The attached host isn't the sandbox's."));
        }
        let scope = controller.scope(&session);
        let existing = workspace.chat(&session).map_err(failed)?;
        if existing.as_ref().is_some_and(|chat| chat.device_id != host && !is_sandbox_host(&chat.device_id)) {
            return Err(refused("This chat runs on another device."));
        }
        let previous_json = workspace.crew_chat_metadata(&session);
        let previous = previous_json
            .clone()
            .map(serde_json::from_value::<Metadata>)
            .transpose()
            .map_err(|_| failed("The chat's Crew metadata isn't Crew's."))?;
        if let Some(previous) = &previous {
            if previous.owner_subject != controller.subject || previous.scope != scope {
                return Err(refused("This chat belongs to another owner or deployment."));
            }
            if let Some(Ok((previous_sandbox, previous_lifecycle, previous_epoch))) = previous.environment.as_ref().map(SessionEnvironment::sandbox) {
                if previous_sandbox != sandbox {
                    return Err(refused("This chat is bound to another sandbox."));
                }
                // Never back to an older epoch, nor (without an explicit
                // Start, which this port doesn't send) out of a stopped one.
                if previous_epoch.is_some_and(|old| old > epoch) || (previous_lifecycle.terminal() && !lifecycle.terminal()) {
                    return Ok(false);
                }
            }
        }
        let route = route
            .cloned()
            .or_else(|| environment.agent_route.clone())
            .or_else(|| previous.as_ref().and_then(|previous| previous.agent_route.clone()));
        let update_config = existing.as_ref().is_none_or(|chat| chat.config.is_none())
            || route.as_ref() != previous.as_ref().and_then(|previous| previous.agent_route.as_ref());
        let mut published = environment.clone();
        // The index's owner is the verified subject (Platform's is an email);
        // activity changes by the minute and is no binding.
        published.owner_principal = controller.subject.clone();
        published.last_activity_at = None;
        let metadata = Metadata {
            owner_subject: controller.subject.clone(),
            scope,
            session_id: session.clone(),
            environment: Some(published),
            agent_route: route.clone(),
        };
        let crew = serde_json::to_value(&metadata).map_err(failed)?;
        let device_missing = !workspace.read_devices().map_err(failed)?.iter().any(|device| device.id == host);
        if !device_missing
            && !update_config
            && previous_json.as_ref() == Some(&crew)
            && existing
                .as_ref()
                .is_some_and(|chat| chat.device_id == host && chat.room_gen == Some(2) && chat.cwd.is_some())
        {
            return Ok(false);
        }
        let whole = existing.is_none();
        let mut chat = existing.unwrap_or_else(|| zeron_proto::Chat {
            id: session.clone(),
            device_id: host.clone(),
            title: environment.name.clone(),
            archived: false,
            cwd: Some(scaffold::HOST_CWD.into()),
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            harness_session_harness: None,
            space_id: None,
            last_seen_at: None,
            room_gen: Some(2),
            parent_chat_id: None,
            child_kind: None,
            sleep: None,
        });
        chat.device_id = host.clone();
        chat.room_gen = Some(2);
        chat.cwd.get_or_insert_with(|| scaffold::HOST_CWD.into());
        chat.config = match (&route, update_config) {
            (Some(route), true) => Some(chat_config(route, chat.config.take())),
            _ if whole => chat.config.take(),
            _ => None,
        };
        let device = device_missing.then(|| Device {
            id: host.clone(),
            name: environment.name.clone().unwrap_or_else(|| "Crew sandbox".into()),
            platform: "linux".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
            cursor_sdk_version: None,
            capabilities: vec![zeron_proto::CLOUD_SESSION_CAPABILITY.into()],
        });
        workspace
            .crew_publish_chat(&chat, &crew, whole, device.as_ref())
            .map_err(failed)?;
        Ok(true)
    }
}

/// A route's chat config: OMP on its model, the pinned account as the
/// chat's `crewAccountId`, full access as Crew's sandboxes run; the rest
/// kept.
fn chat_config(route: &AgentRoute, previous: Option<ChatConfig>) -> ChatConfig {
    let mut config = previous.unwrap_or(ChatConfig {
        harness: route.harness(),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        sandbox: zeron_proto::SandboxLevel::DangerFullAccess,
        access: Default::default(),
        plan: false,
    });
    config.harness = route.harness();
    config.model = Some(route.execution_model());
    config.sandbox = zeron_proto::SandboxLevel::DangerFullAccess;
    match &route.account_id {
        Some(account) => {
            config.model_options.insert("crewAccountId".into(), json!(account));
        }
        None => {
            config.model_options.remove("crewAccountId");
        }
    }
    config
}

impl Outcome {
    fn confirmed(environment: Option<SessionEnvironment>, attachment: Option<Attachment>) -> Self {
        Self { status: "confirmed".into(), environment, attachment, error: None }
    }

    fn rejected(why: &str) -> Self {
        Self { status: "rejected".into(), environment: None, attachment: None, error: Some(why.into()) }
    }
}

fn outcome_result(outcome: Outcome) -> Result<SessionEnvironment, RpcError> {
    match (outcome.status.as_str(), outcome.environment) {
        ("confirmed", Some(environment)) => Ok(environment),
        _ => Err(refused(outcome.error.unwrap_or_else(|| "Crew couldn't start a sandbox.".into()))),
    }
}

/// A sandbox's lifecycle as a Cloud session's state.
fn cloud_state(lifecycle: Lifecycle) -> CloudState {
    match lifecycle {
        Lifecycle::Creating | Lifecycle::RestoringSnapshot => CloudState::Provisioning,
        Lifecycle::Starting | Lifecycle::Resuming => CloudState::Starting,
        Lifecycle::Ready | Lifecycle::AgentRunning => CloudState::Ready,
        Lifecycle::Paused => CloudState::Sleeping,
        Lifecycle::Failed => CloudState::Error,
        Lifecycle::Stopped => CloudState::Off,
    }
}

/// A failed sandbox, as its chat says it (Cloud's alert: "Machine failed"):
/// why, when Platform recorded it (a launch refused before its runtime ran
/// records nothing), and the way on, as Crew can't restart one yet.
fn failed_words(failure: Option<&str>) -> String {
    format!("{} Start a new chat to try again.", failure.unwrap_or("Its sandbox failed."))
}

/// A refusal in a sentence: Platform's own words when it gave some (its
/// code otherwise), after what was being done.
fn words(error: &ScaffoldError, doing: &str) -> String {
    match error.api_error() {
        Some(api) => {
            let why = api.message.clone().unwrap_or_else(|| api.code.replace('_', " "));
            format!("{doing}: {why} ({}).", api.status)
        }
        None => format!("{doing}: {error}."),
    }
}

/// Crew's `guard_scaffold_target`: whether a call forwarded to `target`, a
/// sandbox host, is one the Edge relays to a session-scoped host
/// (`edge/src/crew/authority.ts` `scopedRpcAllowed`), so a refused frame
/// never ends the device link. Any other target passes.
pub(crate) fn guard_scaffold_target(workspace: &WorkspaceHost, target: &str, method: &str, params: &Value) -> Result<(), RpcError> {
    if !is_sandbox_host(target) {
        return Ok(());
    }
    let session = workspace
        .watch_chats()
        .borrow()
        .iter()
        .find(|chat| chat.device_id == target)
        .map(|chat| chat.id.clone());
    if session.is_some_and(|session| host_rpc_allowed(method, params, &session)) {
        return Ok(());
    }
    Err(RpcError::Failed(format!("{method} is unavailable for this session-scoped Scaffold host")))
}

/// The Edge's list for a host's own session (its `session.*` capabilities
/// being Crew's seven).
fn host_rpc_allowed(method: &str, params: &Value, session_id: &str) -> bool {
    if params.get("chatId").and_then(Value::as_str) != Some(session_id) {
        return false;
    }
    fn kind(command: Option<&Value>) -> Option<&str> {
        command?.get("kind")?.as_str()
    }
    let typed = |kind: Option<&str>| matches!(kind, Some("run" | "steer" | "interrupt" | "respondInput"));
    match method {
        methods::WATCH_DOC_MESSAGES
        | methods::WATCH_QUEUE
        | methods::FOCUS_CHAT
        | methods::UPLOAD_CHUNK
        | methods::UPLOAD_COMMIT
        | "ListCrewAnnotations"
        | "ApplyCrewAnnotation"
        // The queue: send while a turn runs, and its edits.
        | methods::QUEUE_MESSAGE
        | methods::UPDATE_QUEUED_MESSAGE
        | methods::BEGIN_QUEUED_MESSAGE_EDIT
        | methods::RENEW_QUEUED_MESSAGE_EDIT
        | methods::FINISH_QUEUED_MESSAGE_EDIT
        | methods::MOVE_QUEUED_MESSAGE
        | methods::REMOVE_QUEUED_MESSAGE
        | methods::SEND_QUEUED_MESSAGE_NOW
        | methods::STEER_QUEUED_MESSAGE_NOW
        | methods::STOP_WAITING
        | methods::STOP_BACKGROUND_TASK => true,
        methods::QUEUE_COMMAND => typed(kind(params.get("command"))),
        methods::RELAY_COMMAND => typed(kind(params.get("entry").and_then(|entry| entry.get("payload")))),
        _ => false,
    }
}

#[cfg(test)]
#[path = "crew_cloud_tests.rs"]
mod tests;
