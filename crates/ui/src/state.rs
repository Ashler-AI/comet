//! App state: the engine connection, entity lists, and the selected chat's
//! transcript — one gpui [`Entity`] the whole shell renders from.
//!
//! ## EngineHandle
//! The UI talks the same typed RPC whether the engine is in-process or a separate
//! daemon (ARCHITECTURE §1). [`EngineHandle::bootstrap`] probes the localhost IPC
//! port, mirroring comet: if an engine is listening it connects over WebSocket
//! ([`RemoteEngine`]); otherwise it embeds one via [`EngineCore::assemble`] and an
//! in-memory RPC transport ([`InProcessEngine`]) — same envelopes, same dispatch.
//!
//! ## Async bridging
//! `bootstrap` runs on tokio via `gpui_tokio::Tokio::spawn`. Once an [`RpcClient`]
//! exists, its `call`/`subscribe` futures are runtime-agnostic (tokio channels),
//! so subscription pumps run on gpui's own executor via `cx.spawn` and fold each
//! frame into the entity with `this.update(...)` + `cx.notify()`.
//!
//! Pure logic (sort order, staleness, gate phase) lives in free functions with
//! unit tests; rendering reads them.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gpui::{App, AsyncApp, Context, Entity, Task, WeakEntity};
use gpui_tokio::Tokio;
use serde::de::DeserializeOwned;

use comet_doc::{MessagePart, MessageRole, SessionMessageEntry, TranscriptDesync, TranscriptFrame};
use comet_engine::{Engine, EngineConfig, EngineRuntime, rpc::AuthRpc};
use comet_proto::{
    AgentRoute, AuthState, Chat, ChatIndicator, CollaborationScope, CollaborationSnapshot, Device,
    HarnessId, LocalSessionAttachResult, LocalSessionCandidate, MessageProvenance,
    ParticipantPresence, RuntimeProfile, ScaffoldEnvironmentControl,
    ScaffoldEnvironmentControlResult, ScaffoldLifecycle, ScaffoldRuntimeMode, Session,
    SessionEnvironmentSource, SessionRef, SessionRoomProjection, Space,
};
use comet_rpc::{RpcClient, RpcError, RpcReply, RpcService, connect_ws, memory_client, methods};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveHarnessGoal {
    pub objective: String,
    pub status: String,
}

/// The newest normalized OMP goal-state carrier wins, including a null goal
/// emitted by `/goal drop`. The transcript persists this hidden part, so every
/// editor surface can project the active goal after navigation or restart.
pub(crate) fn latest_active_omp_goal(entries: &[SessionMessageEntry]) -> Option<ActiveHarnessGoal> {
    for part in entries
        .iter()
        .rev()
        .flat_map(|entry| entry.parts.iter().rev())
    {
        let MessagePart::Tool {
            id,
            call:
                comet_proto::ToolCall::Unknown {
                    name,
                    input: Some(input),
                },
            ..
        } = part
        else {
            continue;
        };
        if id != comet_proto::OMP_GOAL_STATE_CALL_ID
            || name != comet_proto::OMP_GOAL_STATE_CALL_NAME
        {
            continue;
        }
        let goal = input.get("goal")?;
        let objective = goal
            .get("objective")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|objective| !objective.is_empty())?
            .to_string();
        let status = goal
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("active")
            .to_string();
        return Some(ActiveHarnessGoal { objective, status });
    }
    None
}
/// Hidden compatibility artifact from the removed virtual Scaffold space.
/// The synced data remains untouched; the headed app no longer presents it.
const LEGACY_SCAFFOLD_SPACE_ID_PREFIX: &str = "comet-scaffold-space-";

// ---------------------------------------------------------------------------
// Engine handle
// ---------------------------------------------------------------------------

/// Everything needed to reach (or start) an engine.
#[derive(Debug, Clone)]
pub struct EngineBootConfig {
    /// Data directory for the embedded engine (`~/.comet-native`).
    pub data_dir: PathBuf,
    /// Localhost IPC port to probe / serve.
    pub ipc_port: u16,
    /// Edge base URL for the embedded engine.
    pub edge_url: String,
    /// Bearer for edge room joins; `None` runs offline.
    pub edge_token: Option<String>,
    /// Operator-configured Scaffold project/deployment boundary.
    pub project_scope: String,
    /// Trusted deployment namespace for a Scaffold-host SessionRoom.
    pub deployment_id: Option<String>,
    /// Scaffold control-plane origin; `None` keeps explicit local mode.
    pub scaffold_url: Option<String>,
    /// Harness for doc-command runs until per-chat config lands (M4).
    pub default_harness: HarnessId,
    /// Server-enforced capabilities for the engine process.
    pub runtime_profile: RuntimeProfile,
}

/// How this UI reached its engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineMode {
    /// Engine embedded in this process (in-memory RPC transport).
    InProcess,
    /// Connected to a separate daemon over localhost WebSocket.
    Remote { url: String },
}

/// One of the two ways to own an engine connection. Both end at an [`RpcClient`]
/// speaking the identical protocol — the trait only differs in provenance and
/// teardown.
#[async_trait]
trait EngineBackend: Send + Sync {
    fn client(&self) -> &RpcClient;
    fn mode(&self) -> EngineMode;
    /// Graceful teardown (drains runs / flushes docs for the in-process engine).
    async fn shutdown(&self);
}

/// Embedded engine: owns the [`EngineCore`] and an in-memory RPC loop.
struct InProcessEngine {
    runtime: Arc<tokio::sync::Mutex<Option<EngineRuntime>>>,
    boot_task: tokio::task::JoinHandle<()>,
    refresh_task: tokio::task::JoinHandle<()>,
    /// Serves this engine to other viewports over the IPC port. `None` when the
    /// port was already taken — the window still works over its own transport.
    ipc_task: Option<tokio::task::JoinHandle<()>>,
    client: RpcClient,
}

#[async_trait]
impl EngineBackend for InProcessEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::InProcess
    }
    async fn shutdown(&self) {
        self.boot_task.abort();
        // Stop accepting first: a viewport must not connect midway through the
        // drain and queue work against stores that are closing.
        if let Some(ipc) = &self.ipc_task {
            ipc.abort();
        }
        if let Some(runtime) = self.runtime.lock().await.take() {
            runtime.shutdown().await;
        }
        self.refresh_task.abort();
    }
}

#[derive(Clone)]
enum DeferredEngineState {
    Waiting,
    Ready(Arc<dyn RpcService>),
    Failed(String),
}

/// Serves AuthRpc immediately, then holds all data RPC calls until the signed-in
/// user's identity-scoped engine is assembled. Existing UI subscriptions remain
/// pending and attach to the real service without reconnecting.
struct DeferredEngineRpc {
    auth: AuthRpc,
    state: tokio::sync::watch::Receiver<DeferredEngineState>,
}

#[async_trait]
impl RpcService for DeferredEngineRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        if AuthRpc::handles(method) {
            return self.auth.handle(method, params).await;
        }

        let mut state = self.state.clone();
        loop {
            let current = { state.borrow().clone() };
            match current {
                DeferredEngineState::Waiting => {}
                DeferredEngineState::Ready(service) => {
                    return service.handle(method, params).await;
                }
                DeferredEngineState::Failed(message) => return Err(RpcError::Failed(message)),
            }
            state.changed().await.map_err(|_| RpcError::Closed)?;
        }
    }
}

/// External daemon over `ws://127.0.0.1:{port}`.
struct RemoteEngine {
    client: RpcClient,
    url: String,
}

#[async_trait]
impl EngineBackend for RemoteEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::Remote {
            url: self.url.clone(),
        }
    }
    async fn shutdown(&self) {
        // The daemon outlives this viewport; nothing to tear down.
    }
}

/// Cheaply clonable handle to whichever backend won the probe.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<dyn EngineBackend>,
}

impl EngineHandle {
    /// Probe the IPC port and connect (daemon listening) or embed (nothing there).
    /// Must run on the tokio runtime (`Tokio::spawn`): both transports spawn
    /// tokio tasks.
    pub async fn bootstrap(config: EngineBootConfig) -> anyhow::Result<EngineHandle> {
        let url = format!("ws://127.0.0.1:{}", config.ipc_port);
        let probe = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            tokio::net::TcpStream::connect(("127.0.0.1", config.ipc_port)),
        )
        .await;
        if matches!(probe, Ok(Ok(_))) {
            tracing::info!(%url, "engine daemon detected; connecting");
            match connect_ws(&url).await {
                Ok(client) => {
                    return Ok(EngineHandle {
                        inner: Arc::new(RemoteEngine { client, url }),
                    });
                }
                // Something is on the port but it is not an engine (or it is
                // wedged). Fall through and embed: a stranger holding 27654
                // should cost other viewports, not this window.
                Err(err) => tracing::warn!(%url, error = %err, "not an engine; embedding instead"),
            }
        }

        tracing::info!(data_dir = %config.data_dir.display(), "no daemon on port; embedding engine");
        let engine_config = EngineConfig {
            data_dir: config.data_dir,
            edge_url: config.edge_url,
            edge_token: config.edge_token,
            ipc_port: config.ipc_port,
            default_harness: config.default_harness,
            runtime_profile: config.runtime_profile,
            project_scope: config.project_scope,
            deployment_id: config.deployment_id,
            scaffold_url: config.scaffold_url,
        };
        let auth = Engine::build_auth(&engine_config).await;
        let refresh_task = auth.spawn_refresh_loop();
        let (state_tx, state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
        let service: Arc<dyn RpcService> = Arc::new(DeferredEngineRpc {
            auth: AuthRpc::new(auth.clone()),
            state: state_rx,
        });
        let client = memory_client(service.clone());

        // Serve the same service on the IPC port so a terminal viewport can
        // attach to this window's engine with no setup. Deliberately the
        // *deferred* service, not the assembled one: a viewport that connects
        // before sign-in gets AuthRpc (so it can show its own gate) and its
        // data subscriptions wait exactly as this window's do.
        //
        // Best-effort — losing the bind race with another engine costs other
        // viewports, not this one.
        let ipc_task = match comet_engine::serve_ipc(engine_config.ipc_port, service).await {
            Ok(task) => Some(task),
            Err(err) => {
                tracing::warn!(
                    port = engine_config.ipc_port,
                    error = %err,
                    "IPC port unavailable; other viewports cannot attach to this window"
                );
                None
            }
        };
        let runtime = Arc::new(tokio::sync::Mutex::new(None));
        let runtime_for_boot = runtime.clone();
        let boot_task = tokio::spawn(async move {
            let mut auth_state = auth.watch_state();
            while !auth_state.borrow().is_signed_in() {
                if auth_state.changed().await.is_err() {
                    state_tx.send_replace(DeferredEngineState::Failed(
                        "authentication state closed before sign-in".into(),
                    ));
                    return;
                }
            }

            match Engine::assemble_runtime(&engine_config, auth).await {
                Ok(engine_runtime) => {
                    let service: Arc<dyn RpcService> = engine_runtime.core().rpc_service();
                    *runtime_for_boot.lock().await = Some(engine_runtime);
                    state_tx.send_replace(DeferredEngineState::Ready(service));
                }
                Err(err) => {
                    tracing::error!(error = %err, "embedded engine assembly failed");
                    state_tx.send_replace(DeferredEngineState::Failed(format!("{err:#}")));
                }
            }
        });
        Ok(EngineHandle {
            inner: Arc::new(InProcessEngine {
                runtime,
                boot_task,
                refresh_task,
                ipc_task,
                client,
            }),
        })
    }

    pub fn client(&self) -> &RpcClient {
        self.inner.client()
    }

    pub fn mode(&self) -> EngineMode {
        self.inner.mode()
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// Pure state + reducers
// ---------------------------------------------------------------------------

// The frontend-agnostic derivations (sort orders, staleness gating, sidebar
// grouping, the boot gate, relative times) live in `comet_proto::view`, pure
// and with their own test suite. Re-exported here because every call site in
// this crate reads them as `state::…`.
pub use comet_proto::view::{
    ChatGroup, ConnectionStatus, GatePhase, Indicator, SESSION_STALE_MS, attention_rank,
    chat_location, display_status, effective_indicator, format_time_ago, gate_phase, group_chats,
    parse_auth_state, project_label, sort_active, sort_chats, sort_spaces, sort_tabs,
};

/// A compact transcript-derived label for an imported session. The first user
/// turn is stable as the conversation grows; blank/tool-only turns keep the
/// exact-id fallback.
fn shared_session_preview(entries: &[SessionMessageEntry]) -> Option<String> {
    let text = entries
        .iter()
        .find(|entry| entry.role == MessageRole::User)?
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return None;
    }
    let mut chars = one_line.chars();
    let preview: String = chars.by_ref().take(48).collect();
    Some(if chars.next().is_some() {
        format!("{preview}\u{2026}")
    } else {
        preview
    })
}

fn session_ref_fallback(chat_id: &str) -> String {
    format!("Session {}", chat_id.chars().take(8).collect::<String>())
}

// ---------------------------------------------------------------------------

// AppState entity
// ---------------------------------------------------------------------------

/// A local Comet session selected for Scaffold. The sandbox does not exist
/// until this session's first prompt is submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScaffoldSessionDraft {
    pub project_id: String,
    pub deployment_id: String,
    pub space_id: String,
    pub chat_id: String,
}
impl ScaffoldSessionDraft {
    pub fn collaboration_scope(&self) -> CollaborationScope {
        CollaborationScope {
            project_id: self.project_id.clone(),
            deployment_id: Some(self.deployment_id.clone()),
            session_id: Some(self.chat_id.clone()),
            unknown: Default::default(),
        }
    }
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScaffoldControlTarget {
    pub sandbox_id: String,
    pub scope: CollaborationScope,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScaffoldSessionAttachment {
    pub projection: SessionRoomProjection,
    pub grant_id: String,
    pub owner_device_id: String,
    pub actor_subject: String,
    pub source_ref: Option<String>,
    pub control_target: ScaffoldControlTarget,
}

/// Start the staging sandbox without coupling creation to remote Comet
/// readiness. The composer owns the bounded readiness wait and fails closed.
pub(crate) async fn create_scaffold_session(
    handle: &EngineHandle,
    scope: &CollaborationScope,
    source_ref: Option<&str>,
    agent_route: &AgentRoute,
) -> Result<(String, CollaborationScope), RpcError> {
    let create = ScaffoldEnvironmentControl::Create {
        scope: scope.clone(),
        name: Some("Comet Scaffold session".into()),
        source_ref: source_ref.map(str::to_string),
        region: None,
        runtime_mode: Some(ScaffoldRuntimeMode::Compose),
        agent_route: agent_route.clone(),
    };
    let value = handle
        .client()
        .call(
            methods::CONTROL_SCAFFOLD_ENVIRONMENT,
            serde_json::to_value(create).unwrap_or_default(),
        )
        .await?;
    let created: ScaffoldEnvironmentControlResult =
        serde_json::from_value(value).map_err(|err| RpcError::Failed(err.to_string()))?;
    let authoritative_scope = created.environment.scope.clone();
    let SessionEnvironmentSource::Scaffold { sandbox_id, .. } = created.environment.source else {
        return Err(RpcError::Failed(
            "Scaffold returned a local environment".into(),
        ));
    };
    Ok((sandbox_id, authoritative_scope))
}

/// Read the sandbox lifecycle without opening a session-room projection or
/// starting its remote Comet process.
pub(crate) async fn inspect_scaffold_session(
    handle: &EngineHandle,
    sandbox_id: &str,
    scope: &CollaborationScope,
) -> Result<ScaffoldLifecycle, RpcError> {
    let inspect = ScaffoldEnvironmentControl::Inspect {
        sandbox_id: sandbox_id.to_string(),
        scope: scope.clone(),
    };
    let value = handle
        .client()
        .call(
            methods::CONTROL_SCAFFOLD_ENVIRONMENT,
            serde_json::to_value(inspect).unwrap_or_default(),
        )
        .await?;
    let inspected: ScaffoldEnvironmentControlResult =
        serde_json::from_value(value).map_err(|err| RpcError::Failed(err.to_string()))?;
    let SessionEnvironmentSource::Scaffold {
        sandbox_id: inspected_id,
        lifecycle,
        ..
    } = inspected.environment.source
    else {
        return Err(RpcError::Failed(
            "Scaffold inspect returned a local environment".into(),
        ));
    };
    if inspected_id != sandbox_id
        || inspected.environment.scope.project_id != scope.project_id
        || inspected.environment.scope.deployment_id != scope.deployment_id
        || inspected.environment.scope.session_id != scope.session_id
    {
        return Err(RpcError::Failed(
            "Scaffold inspect returned a different sandbox scope".into(),
        ));
    }
    Ok(lifecycle)
}

/// Attach once the sandbox reports a runnable lifecycle. Every successful
/// result is fully validated before its route is installed into UI state.
pub(crate) async fn attach_scaffold_session(
    handle: &EngineHandle,
    sandbox_id: &str,
    scope: CollaborationScope,
) -> Result<ScaffoldSessionAttachment, RpcError> {
    let attach = ScaffoldEnvironmentControl::Attach {
        sandbox_id: sandbox_id.to_string(),
        scope: scope.clone(),
    };
    let value = handle
        .client()
        .call(
            methods::CONTROL_SCAFFOLD_ENVIRONMENT,
            serde_json::to_value(attach).unwrap_or_default(),
        )
        .await?;
    let attached: ScaffoldEnvironmentControlResult =
        serde_json::from_value(value).map_err(|err| RpcError::Failed(err.to_string()))?;
    let source_ref = attached.environment.source_ref.clone();
    let SessionEnvironmentSource::Scaffold {
        sandbox_id: attached_sandbox_id,
        lifecycle_epoch,
        ..
    } = &attached.environment.source
    else {
        return Err(RpcError::Failed(
            "Scaffold attach returned a local environment".into(),
        ));
    };
    if attached_sandbox_id != sandbox_id {
        return Err(RpcError::Failed(
            "Scaffold attach returned a different sandbox".into(),
        ));
    }
    let projection = attached
        .room_projection
        .ok_or_else(|| RpcError::Failed("Scaffold attach returned no session room".into()))?;
    if projection.project_id != scope.project_id
        || Some(projection.deployment_id.as_str()) != scope.deployment_id.as_deref()
        || Some(projection.session_id.as_str()) != scope.session_id.as_deref()
    {
        return Err(RpcError::Failed(
            "Scaffold attach returned a different session room".into(),
        ));
    }
    let grant = attached
        .control_grant
        .ok_or_else(|| RpcError::Failed("Scaffold attach returned no control grant".into()))?;
    if !grant
        .capabilities
        .iter()
        .any(|capability| capability == comet_proto::CAPABILITY_SESSION_CHAT)
    {
        return Err(RpcError::Failed(
            "Scaffold attach returned no chat authority".into(),
        ));
    }
    let expected_lifecycle_epoch = lifecycle_epoch
        .ok_or_else(|| RpcError::Failed("Scaffold attach returned no lifecycle epoch".into()))?;
    let owner_device_id = attached
        .attached_device_id
        .filter(|device_id| {
            comet_proto::parse_scaffold_device_id(device_id).is_some_and(
                |(device_sandbox_id, lifecycle_epoch)| {
                    device_sandbox_id == sandbox_id && lifecycle_epoch == expected_lifecycle_epoch
                },
            )
        })
        .ok_or_else(|| RpcError::Failed("Scaffold attach returned a different device".into()))?;
    if attached.environment.owner_principal.is_empty() {
        return Err(RpcError::Failed(
            "Scaffold attach returned no owner identity".into(),
        ));
    }
    Ok(ScaffoldSessionAttachment {
        projection,
        grant_id: grant.id,
        owner_device_id,
        actor_subject: attached.environment.owner_principal,
        source_ref,
        control_target: ScaffoldControlTarget {
            sandbox_id: sandbox_id.to_string(),
            scope,
        },
    })
}
const SCAFFOLD_ATTACH_MAX_ATTEMPTS: usize = 60;
#[cfg(not(test))]
const SCAFFOLD_ATTACH_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
#[cfg(test)]
const SCAFFOLD_ATTACH_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(1);

fn is_retryable_scaffold_attach_error(error: &RpcError) -> bool {
    matches!(
        error,
        RpcError::Failed(message)
            if message.contains("scaffold_api_error:502:scaffold_request_rejected")
                || message.contains(
                    "scaffold_api_error:409:sandbox_provider_error:Sandbox lifecycle changed while the operation was in flight",
                )
    )
}

async fn attach_scaffold_session_with_retry<Wait, WaitFuture>(
    handle: &EngineHandle,
    sandbox_id: &str,
    scope: CollaborationScope,
    wait: &Wait,
) -> Result<ScaffoldSessionAttachment, RpcError>
where
    Wait: Fn(std::time::Duration) -> WaitFuture,
    WaitFuture: Future<Output = ()>,
{
    let mut attempt = 1;
    loop {
        match attach_scaffold_session(handle, sandbox_id, scope.clone()).await {
            Ok(attachment) => return Ok(attachment),
            Err(error)
                if attempt < SCAFFOLD_ATTACH_MAX_ATTEMPTS
                    && is_retryable_scaffold_attach_error(&error) =>
            {
                attempt += 1;
                wait(SCAFFOLD_ATTACH_RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Reconnect an existing Scaffold room. Paused sandboxes resume before a fresh,
/// lifecycle-bound device grant is installed; active sandboxes only reattach.
pub(crate) async fn ensure_scaffold_session_attached<Wait, WaitFuture>(
    handle: &EngineHandle,
    sandbox_id: &str,
    scope: &CollaborationScope,
    wait: &Wait,
) -> Result<ScaffoldSessionAttachment, RpcError>
where
    Wait: Fn(std::time::Duration) -> WaitFuture,
    WaitFuture: Future<Output = ()>,
{
    match inspect_scaffold_session(handle, sandbox_id, scope).await? {
        ScaffoldLifecycle::Paused => {
            let value = handle
                .client()
                .call(
                    methods::CONTROL_SCAFFOLD_ENVIRONMENT,
                    serde_json::to_value(ScaffoldEnvironmentControl::Resume {
                        sandbox_id: sandbox_id.to_string(),
                        scope: scope.clone(),
                    })
                    .unwrap_or_default(),
                )
                .await?;
            let resumed: ScaffoldEnvironmentControlResult =
                serde_json::from_value(value).map_err(|err| RpcError::Failed(err.to_string()))?;
            if resumed.environment.scope != *scope {
                return Err(RpcError::Failed(
                    "Scaffold resume returned a different sandbox scope".into(),
                ));
            }
            let SessionEnvironmentSource::Scaffold {
                sandbox_id: resumed_id,
                lifecycle,
                ..
            } = resumed.environment.source
            else {
                return Err(RpcError::Failed(
                    "Scaffold resume returned a local environment".into(),
                ));
            };
            if resumed_id != sandbox_id {
                return Err(RpcError::Failed(
                    "Scaffold resume returned a different sandbox scope".into(),
                ));
            }
            if matches!(
                lifecycle,
                ScaffoldLifecycle::Stopped | ScaffoldLifecycle::Failed
            ) {
                return Err(RpcError::Failed(format!(
                    "Scaffold resume returned terminal lifecycle {lifecycle:?}"
                )));
            }
        }
        ScaffoldLifecycle::Stopped | ScaffoldLifecycle::Failed => {
            return Err(RpcError::Failed(
                "Scaffold session is no longer resumable".into(),
            ));
        }
        ScaffoldLifecycle::Creating
        | ScaffoldLifecycle::RestoringSnapshot
        | ScaffoldLifecycle::Starting
        | ScaffoldLifecycle::Ready
        | ScaffoldLifecycle::AgentRunning
        | ScaffoldLifecycle::Resuming => {}
    }
    attach_scaffold_session_with_retry(handle, sandbox_id, scope.clone(), wait).await
}

/// Create the sandbox and immediately dispatch its supervised Comet bootstrap.
/// Runtime readiness depends on this attachment, so callers must not wait for
/// `Ready` before attaching.
pub(crate) async fn create_and_attach_scaffold_session<Wait, WaitFuture>(
    handle: &EngineHandle,
    scope: &CollaborationScope,
    source_ref: Option<&str>,
    agent_route: &AgentRoute,
    wait: &Wait,
) -> Result<(String, ScaffoldSessionAttachment), RpcError>
where
    Wait: Fn(std::time::Duration) -> WaitFuture,
    WaitFuture: Future<Output = ()>,
{
    let (sandbox_id, authoritative_scope) =
        create_scaffold_session(handle, scope, source_ref, agent_route).await?;
    let attachment =
        attach_scaffold_session_with_retry(handle, &sandbox_id, authoritative_scope, wait).await?;
    Ok((sandbox_id, attachment))
}
/// Pause exactly the sandbox attached to a chat. The response must preserve
/// both physical sandbox identity and logical session scope.
pub(crate) async fn pause_scaffold_session(
    handle: &EngineHandle,
    target: &ScaffoldControlTarget,
) -> Result<(), RpcError> {
    let value = handle
        .client()
        .call(
            methods::CONTROL_SCAFFOLD_ENVIRONMENT,
            serde_json::to_value(ScaffoldEnvironmentControl::Pause {
                sandbox_id: target.sandbox_id.clone(),
                scope: target.scope.clone(),
            })
            .unwrap_or_default(),
        )
        .await?;
    let paused: ScaffoldEnvironmentControlResult =
        serde_json::from_value(value).map_err(|err| RpcError::Failed(err.to_string()))?;
    if paused.environment.scope != target.scope {
        return Err(RpcError::Failed(
            "Scaffold pause returned a different sandbox scope".into(),
        ));
    }
    let SessionEnvironmentSource::Scaffold {
        sandbox_id,
        lifecycle,
        ..
    } = paused.environment.source
    else {
        return Err(RpcError::Failed(
            "Scaffold pause returned a local environment".into(),
        ));
    };
    if sandbox_id != target.sandbox_id || lifecycle != ScaffoldLifecycle::Paused {
        return Err(RpcError::Failed(
            "Scaffold pause did not pause the attached sandbox".into(),
        ));
    }
    Ok(())
}
pub(crate) async fn archive_and_pause_scaffold_session(
    handle: &EngineHandle,
    chat_id: &str,
    target: &ScaffoldControlTarget,
) -> Result<(), RpcError> {
    handle
        .client()
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatArchived",
                "chatId": chat_id,
                "archived": true,
            }),
        )
        .await?;
    pause_scaffold_session(handle, target).await
}

/// Root application state. Reducer methods (`apply_*`, [`Self::session_for`], …)
/// are plain `&mut self` functions so tests construct the struct directly; gpui
/// glue ([`Self::bootstrap`], [`Self::select_chat`]) layers subscriptions on top.
pub struct AppState {
    pub connection: ConnectionStatus,
    /// Auth stream value; `None` until the engine reports one (M4).
    pub auth: Option<AuthState>,
    pub devices: Vec<Device>,
    /// Sorted (see [`sort_spaces`]).
    pub spaces: Vec<Space>,
    /// Sorted (see [`sort_chats`]); includes archived rows — views filter.
    pub chats: Vec<Chat>,
    /// Harness-native session metadata discovered on this device. Transcripts
    /// remain engine-private until the user attaches one candidate explicitly.
    pub local_session_candidates: Vec<LocalSessionCandidate>,
    pub local_sessions_loading: bool,
    pub local_sessions_error: Option<String>,
    pub local_session_attaching: HashSet<String>,
    pub local_session_attach_errors: HashMap<String, String>,
    local_sessions_refreshed_at: Option<std::time::Instant>,
    /// Session attachment and first-send creation can resolve before the chats
    /// watch publishes their row. Keep the selected id alive until it arrives.
    pending_local_chat_ids: HashSet<String>,
    pub sessions: Vec<Session>,
    /// Imported session memberships from the workspace `sessionRefs` map.
    pub session_refs: Vec<SessionRef>,
    /// Transcript-derived labels learned after an imported room has opened.
    shared_session_previews: HashMap<String, String>,
    /// The space whose tabs fill the main area. Healed by [`Self::apply_spaces`]
    /// when the row vanishes; selecting a chat implies its space.
    pub selected_space: Option<String>,
    /// Presentation-only members of the selected logical sidebar source.
    /// Persisted spaces and chat ownership remain unchanged.
    selected_space_members: Vec<String>,
    pub selected_chat: Option<String>,
    /// Boot auto-select happened (or a manual selection superseded it).
    pub auto_selected: bool,
    /// Joined transcript of the selected chat (continuations folded engine-side).
    pub transcript: Vec<SessionMessageEntry>,
    /// Multiplayer projection for the selected shared thread. The last good
    /// snapshot remains visible while its watch reconnects or a model hands off.
    pub collaboration: Option<CollaborationSnapshot>,
    /// Independently-owned agent session targeted by pause/resume/steer/stop.
    /// It is UI selection only; authority still comes from the verified grant.
    pub selected_agent_session: Option<String>,
    /// Installed-app invitation awaiting the exact session/grant projection.
    pending_invitation: Option<comet_proto::CometInvitation>,
    /// Grant named by the accepted deep link. It remains a routing identity;
    /// command authority is still checked against the verified projection.
    pub selected_invitation_grant: Option<String>,
    /// Verified invite membership awaiting its `AddSessionRef` pin — set once
    /// the deep link's grant checks out against the room projection and the
    /// session has no workspace chat row. Drained wherever a `Context` and the
    /// engine handle are both in reach.
    pending_session_pin: Option<String>,
    /// Trusted per-chat room scopes returned by Scaffold Attach. Absence is
    /// intentional: ordinary local sessions continue to use legacy s3 rooms.
    room_projections: HashMap<String, SessionRoomProjection>,
    /// Exact non-secret grant id returned by the attach that selected each
    /// Scaffold room. It is a route selector only; authority remains host-local.
    scaffold_control_grants: HashMap<String, String>,
    /// Non-secret physical targets for attached Scaffold chats. Retained after
    /// settlement so archive can pause the exact sandbox idempotently.
    scaffold_control_targets: HashMap<String, ScaffoldControlTarget>,
    /// Exact local Comet session awaiting its first-prompt Scaffold attach.
    pending_scaffold_session: Option<ScaffoldSessionDraft>,
    /// A Comet chat row is being persisted for a user-selected Scaffold session.
    scaffold_session_creating: bool,
    pub scaffold_session_error: Option<String>,
    /// Configured local-controller boundary for creating Scaffold demo sandboxes.
    scaffold_scope: Option<(String, String)>,
    /// First turns intentionally wait here after the staging sandbox is accepted.
    scaffold_starting_chats: HashSet<String>,
    /// Chats the user explicitly marked unread. Suppresses the shell's
    /// "looking at it ⇒ seen" auto-stamp until the user actively re-selects
    /// the chat, and pins the row unseen against in-flight watch frames that
    /// still carry the pre-mutate seen stamp.
    unread_marks: HashSet<String>,
    /// Optimistic user echoes per chat id, shown until the doc frame carrying
    /// the same message id arrives (client-minted ids make dedup exact).
    echoes: HashMap<String, Vec<SessionMessageEntry>>,
    /// This engine's device id (best-effort `LocalDevice` probe; `None` until
    /// the engine serves it — views degrade gracefully).
    pub local_device_id: Option<String>,
    /// Latest `UpdateStatus` frame — drives the sidebar update strip.
    pub update: Option<comet_update::UpdateStatus>,
    /// Data directory (`ui-settings.json`, `composer-defaults.json`); set at
    /// bootstrap so child views can persist small preference files.
    pub data_dir: Option<PathBuf>,
    runtime_profile: RuntimeProfile,
    engine: Option<EngineHandle>,
    /// Boot config retained so a closed RPC transport can re-run the
    /// probe-or-embed bootstrap (`on_engine_closed`).
    boot_config: Option<EngineBootConfig>,
    /// Quit in progress: the drained in-process engine's closing streams must
    /// not trigger the reconnect supervisor mid-shutdown.
    shutting_down: bool,
    watch_tasks: Vec<Task<()>>,
    transcript_task: Option<Task<()>>,
    collaboration_task: Option<Task<()>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
fn configured_scaffold_scope(config: &EngineBootConfig) -> Option<(String, String)> {
    config.scaffold_url.as_ref().map(|_| {
        let deployment_id = config
            .deployment_id
            .clone()
            .unwrap_or_else(|| config.project_scope.clone());
        (config.project_scope.clone(), deployment_id)
    })
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connection: ConnectionStatus::Connecting,
            auth: None,
            devices: Vec::new(),
            spaces: Vec::new(),
            chats: Vec::new(),
            local_session_candidates: Vec::new(),
            local_sessions_loading: false,
            local_sessions_error: None,
            local_session_attaching: HashSet::new(),
            local_session_attach_errors: HashMap::new(),
            local_sessions_refreshed_at: None,
            pending_local_chat_ids: HashSet::new(),
            sessions: Vec::new(),
            session_refs: Vec::new(),
            shared_session_previews: HashMap::new(),
            selected_space: None,
            selected_space_members: Vec::new(),
            selected_chat: None,
            transcript: Vec::new(),
            collaboration: None,
            selected_agent_session: None,
            pending_invitation: None,
            selected_invitation_grant: None,
            pending_session_pin: None,
            room_projections: HashMap::new(),
            scaffold_control_grants: HashMap::new(),
            scaffold_control_targets: HashMap::new(),
            pending_scaffold_session: None,
            scaffold_session_creating: false,
            scaffold_session_error: None,
            scaffold_scope: None,
            scaffold_starting_chats: HashSet::new(),
            unread_marks: HashSet::new(),
            echoes: HashMap::new(),
            local_device_id: None,
            update: None,
            data_dir: None,
            runtime_profile: RuntimeProfile::LocalController,
            engine: None,
            boot_config: None,
            shutting_down: false,
            watch_tasks: Vec::new(),
            transcript_task: None,
            collaboration_task: None,
            auto_selected: false,
        }
    }

    // ---- reducers (pure) ----

    pub fn apply_chats(&mut self, mut chats: Vec<Chat>) {
        chats.retain(|chat| {
            !chat
                .space_id
                .as_deref()
                .is_some_and(|id| id.starts_with(LEGACY_SCAFFOLD_SPACE_ID_PREFIX))
        });
        sort_chats(&mut chats);
        // An explicit "mark unread" must survive watch frames that raced the
        // mutate (they still carry the pre-clear seen stamp). Once the synced
        // row itself reads unseen, the pin has served its display purpose —
        // it stays set only to keep suppressing the shell's auto-seen stamp.
        if !self.unread_marks.is_empty() {
            self.unread_marks
                .retain(|id| chats.iter().any(|chat| chat.id == *id));
            for chat in &mut chats {
                if self.unread_marks.contains(&chat.id) {
                    chat.last_seen_at = None;
                }
            }
        }
        self.chats = chats;
        let chats = &self.chats;
        self.local_session_candidates
            .retain(|candidate| !chats.iter().any(|chat| chat.id == candidate.chat_id));
        let candidates = &self.local_session_candidates;
        self.local_session_attach_errors.retain(|candidate_id, _| {
            candidates
                .iter()
                .any(|candidate| candidate.id == *candidate_id)
        });
        self.pending_local_chat_ids
            .retain(|chat_id| !chats.iter().any(|chat| chat.id == *chat_id));
        self.scaffold_starting_chats.retain(|chat_id| {
            chats.iter().any(|chat| chat.id == *chat_id)
                || self.pending_local_chat_ids.contains(chat_id)
        });
        self.heal_selected_session();
    }

    pub fn apply_local_session_candidates(&mut self, mut candidates: Vec<LocalSessionCandidate>) {
        candidates.retain(|candidate| {
            !self.chats.iter().any(|chat| chat.id == candidate.chat_id)
                && !self.pending_local_chat_ids.contains(&candidate.chat_id)
        });
        self.local_session_attach_errors.retain(|candidate_id, _| {
            candidates
                .iter()
                .any(|candidate| candidate.id == *candidate_id)
        });
        self.local_session_candidates = candidates;
    }

    pub fn apply_sessions(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
    }

    pub fn apply_session_refs(&mut self, mut refs: Vec<SessionRef>) {
        refs.sort_by(|a, b| {
            b.added_at
                .cmp(&a.added_at)
                .then_with(|| a.chat_id.cmp(&b.chat_id))
        });
        self.session_refs = refs;
        self.heal_selected_session();
    }

    fn heal_selected_session(&mut self) {
        let Some(selected) = self.selected_chat.as_deref() else {
            return;
        };
        let available = self.chats.iter().any(|chat| chat.id == selected)
            || self
                .session_refs
                .iter()
                .any(|session_ref| session_ref.chat_id == selected)
            || self.pending_local_chat_ids.contains(selected)
            || self
                .pending_invitation
                .as_ref()
                .is_some_and(|invitation| invitation.chat_id == selected);
        if available {
            return;
        }
        self.selected_chat = None;
        self.transcript.clear();
        self.transcript_task = None;
        self.collaboration = None;
        self.collaboration_task = None;
    }

    pub fn apply_spaces(&mut self, mut spaces: Vec<Space>) {
        spaces.retain(|space| !space.id.starts_with(LEGACY_SCAFFOLD_SPACE_ID_PREFIX));
        sort_spaces(&mut spaces);
        self.spaces = spaces;
        // Heal a vanished selection (space deleted elsewhere): fall back to the
        // first space; its chats died with it, so a matching chat selection is
        // healed by the accompanying chats frame (`apply_chats`).
        if let Some(selected) = &self.selected_space
            && !self.spaces.iter().any(|space| &space.id == selected)
        {
            self.selected_space = self.spaces.first().map(|space| space.id.clone());
            self.selected_space_members.clear();
        }
        // First frame with no selection yet: pick the first space so the shell
        // never renders an empty main area while spaces exist.
        if self.selected_space.is_none() {
            self.selected_space = self.spaces.first().map(|space| space.id.clone());
        }
        self.selected_space_members
            .retain(|id| self.spaces.iter().any(|space| space.id == *id));
        if let Some(selected) = self.selected_space.clone()
            && !self.selected_space_members.contains(&selected)
        {
            self.selected_space_members.push(selected);
        }
        self.selected_space_members.sort();
        self.selected_space_members.dedup();
    }

    /// Optimistic local echo of a `setChatConfig` mutate: stamp the row now so
    /// the chips update on click; the next chats watch frame carries the same
    /// value once the engine applies the LWW write.
    pub fn apply_chat_config(&mut self, chat_id: &str, config: comet_proto::ChatConfig) {
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.config = Some(config);
        }
    }

    pub fn apply_devices(&mut self, devices: Vec<Device>) {
        self.devices = devices;
    }

    pub fn apply_update(&mut self, status: comet_update::UpdateStatus) {
        self.update = Some(status);
    }

    pub fn apply_auth(&mut self, auth: AuthState) {
        self.auth = Some(auth);
    }

    /// Tolerant AuthStatus frame reducer (see [`parse_auth_state`]).
    pub fn apply_auth_value(&mut self, value: serde_json::Value) {
        match parse_auth_state(&value) {
            Some(auth) => self.apply_auth(auth),
            None => tracing::warn!("dropping unrecognized AuthStatus frame"),
        }
    }

    /// The signed-in user, if the engine reports one.
    pub fn auth_user(&self) -> Option<&comet_proto::UserProfile> {
        match self.auth.as_ref()? {
            AuthState::SignedIn { user, .. } => Some(user),
            AuthState::SignedOut => None,
        }
    }

    pub fn apply_transcript(&mut self, entries: Vec<SessionMessageEntry>) {
        // Doc frames supersede optimistic echoes carrying the same id.
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            echoes.retain(|echo| !entries.iter().any(|e| e.id == echo.id));
        }
        self.transcript = entries;
        self.update_selected_shared_preview();
    }

    pub fn apply_collaboration(&mut self, snapshot: CollaborationSnapshot) {
        if self.apply_pending_invitation(&snapshot) {
            self.collaboration = Some(snapshot);
            return;
        }
        let selected_still_exists = self.selected_agent_session.as_deref().is_some_and(|id| {
            snapshot
                .sessions
                .iter()
                .any(|session| session.session_id == id)
        });
        if !selected_still_exists {
            let principal = snapshot
                .principal
                .as_ref()
                .map(|principal| principal.subject.as_str());
            self.selected_agent_session = snapshot
                .sessions
                .iter()
                .find(|session| {
                    principal == Some(session.owner_subject.as_str())
                        && self.local_device_id.as_deref() == Some(session.owner_device_id.as_str())
                })
                .or_else(|| snapshot.sessions.first())
                .map(|session| session.session_id.clone());
        }
        self.collaboration = Some(snapshot);
    }

    fn apply_pending_invitation(&mut self, snapshot: &CollaborationSnapshot) -> bool {
        let Some(invitation) = self.pending_invitation.as_ref() else {
            return false;
        };
        if self.selected_chat.as_deref() != Some(invitation.chat_id.as_str()) {
            return false;
        }
        let Some(session) = snapshot.sessions.iter().find(|session| {
            session.chat_id == invitation.chat_id && session.session_id == invitation.session_id
        }) else {
            return true;
        };
        // Navigation may select the named session immediately; this is display
        // state only. The grant remains unavailable until the authenticated
        // projection below proves the exact id and scope.
        self.selected_agent_session = Some(session.session_id.clone());
        let now = Utc::now().timestamp_millis();
        let Some(principal) = snapshot.principal.as_ref() else {
            return true;
        };
        let exact_grant = snapshot.grants.iter().find(|grant| {
            grant.id == invitation.grant_id
                && grant.principal_subject == principal.subject
                && grant.scope.project_id == principal.project_id
                && grant.scope.session_id.as_deref() == Some(session.session_id.as_str())
                && grant.device_id.as_deref() == Some(session.owner_device_id.as_str())
                && grant.granted_at <= now
                && grant.expires_at.is_some_and(|expires| now < expires)
                && grant.revoked_at.is_none()
        });
        let Some(grant) = exact_grant else {
            return true;
        };
        self.selected_invitation_grant = Some(grant.id.clone());
        // The one-click link is the whole membership flow: once the grant
        // verifies and the session has no workspace chat row, pin the imported
        // room so it survives restarts (`drain_session_pin` issues the RPC).
        if !self.has_chat_row(&invitation.chat_id)
            && !self
                .session_refs
                .iter()
                .any(|session_ref| session_ref.chat_id == invitation.chat_id)
        {
            self.pending_session_pin = Some(invitation.chat_id.clone());
        }
        self.pending_invitation = None;
        true
    }

    /// Apply a `WatchDocMessages` delta frame in place. `Err` = this copy has
    /// diverged; the watch task resubscribes for a fresh reset.
    pub fn apply_transcript_frame(
        &mut self,
        frame: TranscriptFrame,
    ) -> Result<(), TranscriptDesync> {
        comet_doc::apply_transcript_frame(&mut self.transcript, frame)?;
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            let transcript = &self.transcript;
            echoes.retain(|echo| !transcript.iter().any(|e| e.id == echo.id));
        }
        self.update_selected_shared_preview();
        Ok(())
    }

    /// Add an optimistic user echo (composer send path).
    pub fn push_echo(&mut self, chat_id: &str, entry: SessionMessageEntry) {
        let echoes = self.echoes.entry(chat_id.to_string()).or_default();
        if !echoes.iter().any(|e| e.id == entry.id) {
            echoes.push(entry);
        }
    }

    /// Hold a newly minted chat selection across chats-watch frames until its
    /// create mutation materializes the row.
    pub fn mark_chat_pending(&mut self, chat_id: &str) {
        if !self.chats.iter().any(|chat| chat.id == chat_id) {
            self.pending_local_chat_ids.insert(chat_id.to_string());
        }
    }

    /// Release a failed first-send reservation. If no row ever materialized,
    /// return to the new-session canvas instead of leaving a ghost selection.
    pub fn cancel_pending_chat(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        self.pending_local_chat_ids.remove(chat_id);
        if self.selected_chat.as_deref() == Some(chat_id)
            && !self.chats.iter().any(|chat| chat.id == chat_id)
        {
            self.select_chat(None, cx);
        }
    }

    /// Drop an echo (send failed — the prompt returns to the draft).
    pub fn remove_echo(&mut self, chat_id: &str, message_id: &str) {
        if let Some(echoes) = self.echoes.get_mut(chat_id) {
            echoes.retain(|e| e.id != message_id);
        }
    }

    /// Unconfirmed echoes for the selected chat, in send order.
    pub fn pending_echoes(&self) -> &[SessionMessageEntry] {
        self.selected_chat
            .as_deref()
            .and_then(|id| self.echoes.get(id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn update_selected_shared_preview(&mut self) {
        let Some(chat_id) = self.selected_chat.as_deref() else {
            return;
        };
        if self.chats.iter().any(|chat| chat.id == chat_id)
            || self.shared_session_previews.contains_key(chat_id)
        {
            return;
        }
        if let Some(preview) = shared_session_preview(&self.transcript) {
            self.shared_session_previews
                .insert(chat_id.to_string(), preview);
        }
    }

    pub fn collaboration_sessions(
        &self,
        chat_id: &str,
    ) -> impl Iterator<Item = &comet_proto::AgentSessionRecord> {
        self.collaboration
            .iter()
            .flat_map(|snapshot| snapshot.sessions.iter())
            .filter(move |session| session.chat_id == chat_id)
    }

    pub fn message_provenance(&self, message_id: &str) -> Option<&MessageProvenance> {
        self.collaboration
            .as_ref()?
            .message_provenance
            .iter()
            .find(|provenance| provenance.message_id == message_id)
    }

    pub fn participants(&self) -> &[ParticipantPresence] {
        self.collaboration
            .as_ref()
            .map(|snapshot| snapshot.participants.as_slice())
            .unwrap_or_default()
    }

    pub fn participant_name<'a>(&'a self, subject: &'a str) -> &'a str {
        self.participants()
            .iter()
            .find(|participant| participant.principal_subject == subject)
            .and_then(|participant| participant.display_name.as_deref())
            .unwrap_or(subject)
    }

    pub fn principal_subject(&self) -> Option<&str> {
        self.collaboration
            .as_ref()?
            .principal
            .as_ref()
            .map(|principal| principal.subject.as_str())
    }

    pub fn has_collaboration_capability(&self, capability: &str) -> bool {
        self.collaboration
            .as_ref()
            .and_then(|snapshot| snapshot.principal.as_ref())
            .is_some_and(|principal| principal.has_capability(capability))
    }

    pub fn selected_agent_session(&self) -> Option<&comet_proto::AgentSessionRecord> {
        let selected = self.selected_agent_session.as_deref()?;
        self.collaboration
            .as_ref()?
            .sessions
            .iter()
            .find(|session| session.session_id == selected)
    }

    pub fn select_agent_session(&mut self, session_id: Option<String>) {
        self.selected_agent_session = session_id.filter(|id| {
            self.collaboration.as_ref().is_some_and(|snapshot| {
                snapshot
                    .sessions
                    .iter()
                    .any(|session| &session.session_id == id)
            })
        });
    }

    pub fn open_invitation(
        &mut self,
        invitation: comet_proto::CometInvitation,
        cx: &mut Context<Self>,
    ) {
        let chat_id = invitation.chat_id.clone();
        self.pending_invitation = Some(invitation);
        self.selected_invitation_grant = None;
        if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
            if let Some(snapshot) = self.collaboration.clone() {
                self.apply_pending_invitation(&snapshot);
                self.drain_session_pin(cx);
            }
            cx.notify();
        } else {
            self.select_chat(Some(chat_id), cx);
        }
    }

    /// Issue the `AddSessionRef` pin recorded by a verified invitation. The
    /// merged result lands immediately (the `WatchSessionRefs` frame carrying
    /// it is the durable source); a lost engine keeps the pin armed for the
    /// next snapshot.
    fn drain_session_pin(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self.pending_session_pin.clone() else {
            return;
        };
        let Some(handle) = self.engine().cloned() else {
            return;
        };
        self.pending_session_pin = None;
        cx.spawn(async move |this, cx| {
            let result = handle
                .client()
                .call(
                    methods::ADD_SESSION_REF,
                    serde_json::json!({ "chatId": chat_id.clone() }),
                )
                .await;
            match result.map(serde_json::from_value::<SessionRef>) {
                Ok(Ok(session_ref)) => {
                    this.update(cx, |state, cx| {
                        let mut refs = state.session_refs.clone();
                        refs.retain(|item| item.chat_id != session_ref.chat_id);
                        refs.push(session_ref);
                        state.apply_session_refs(refs);
                        cx.notify();
                    })
                    .ok();
                }
                Ok(Err(err)) => {
                    tracing::warn!(chat = %chat_id, error = %err, "membership pin returned invalid data");
                }
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "membership pin failed");
                }
            }
        })
        .detach();
    }

    // ---- queries ----

    /// Non-archived chats in sidebar order. A chat row always renders in its
    /// space, even when an exact-id membership pin names the same session —
    /// the row carries the context (status, harness, branch) a bare ref lacks.
    pub fn visible_chats(&self) -> impl Iterator<Item = &Chat> {
        self.chats.iter().filter(|chat| !chat.archived)
    }
    /// Imported memberships with no workspace chat row — sessions genuinely
    /// foreign to this workspace. Row-backed pins are served by the normal
    /// space list instead.
    pub fn shared_session_refs(&self) -> impl Iterator<Item = &SessionRef> {
        self.session_refs
            .iter()
            .filter(|session_ref| !self.has_chat_row(&session_ref.chat_id))
    }

    /// An imported session room without a local chat row: transcript comes
    /// from the room, sends go over steer, attachments are unavailable.
    pub fn is_shared_session(&self, chat_id: &str) -> bool {
        !self.has_chat_row(chat_id)
            && self
                .session_refs
                .iter()
                .any(|session_ref| session_ref.chat_id == chat_id)
    }

    fn has_chat_row(&self, chat_id: &str) -> bool {
        self.chats.iter().any(|chat| chat.id == chat_id)
    }

    pub fn shared_session_title(&self, chat_id: &str) -> String {
        self.shared_session_previews
            .get(chat_id)
            .cloned()
            .unwrap_or_else(|| session_ref_fallback(chat_id))
    }

    pub fn selected_space_row(&self) -> Option<&Space> {
        let id = self.selected_space.as_deref()?;
        self.spaces.iter().find(|s| s.id == id)
    }

    pub fn space_row(&self, space_id: &str) -> Option<&Space> {
        self.spaces.iter().find(|s| s.id == space_id)
    }

    pub fn space_for_chat(&self, chat: &Chat) -> Option<&Space> {
        self.space_row(chat.space_id.as_deref()?)
    }

    /// Non-archived chats of a space in tab (creation) order. Chats with a
    /// dangling/missing `space_id` are invisible by construction.
    pub fn chats_in_space(&self, space_id: &str) -> Vec<&Chat> {
        let mut chats: Vec<&Chat> = self
            .visible_chats()
            .filter(|c| c.space_id.as_deref() == Some(space_id))
            .collect();
        sort_tabs(&mut chats);
        chats
    }

    /// Non-archived chats belonging to every persisted member of the currently
    /// selected logical sidebar source.
    pub fn chats_in_selected_source(&self) -> Vec<&Chat> {
        let mut chats: Vec<&Chat> = self
            .visible_chats()
            .filter(|chat| {
                chat.space_id
                    .as_ref()
                    .is_some_and(|id| self.selected_space_members.contains(id))
            })
            .collect();
        sort_tabs(&mut chats);
        chats
    }

    pub fn device_name(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.as_str())
    }

    /// Host-presence check: is this device's 15s presence heartbeat fresh?
    /// Distinguishes "host offline" (its queued work syncs when it returns)
    /// from slow sync. The local device is trivially online; unknown devices
    /// get the benefit of the doubt (no evidence — don't cry wolf).
    pub fn device_online(&self, device_id: &str, now: DateTime<Utc>) -> bool {
        if self.local_device_id.as_deref() == Some(device_id) {
            return true;
        }
        match self.devices.iter().find(|d| d.id == device_id) {
            Some(d) => crate::settings::devices::device_online(d.last_seen_at, now),
            None => true,
        }
    }

    /// Does the selected space's folder have git? Drives the branch picker and
    /// the diff sidebar (owner-stamped, synced — no RPC).
    pub fn selected_space_git(&self) -> bool {
        self.selected_space_row().is_some_and(|s| s.git_detected)
    }

    /// Full display status for a chat (tab dots, Active list).
    pub fn display_status_for(&self, chat: &Chat, now: DateTime<Utc>) -> ChatIndicator {
        display_status(chat, self.session_for(&chat.id), now)
    }

    /// The sidebar's Sessions list: every non-archived chat of a LIVE space,
    /// on any device — idle included — in pure recency order (status drives
    /// the dot, never the position; see [`sort_active`]).
    pub fn overview_chats(&self, now: DateTime<Utc>) -> Vec<(ChatIndicator, &Chat)> {
        let mut rows: Vec<(ChatIndicator, &Chat)> = self
            .visible_chats()
            .filter(|c| {
                c.space_id
                    .as_deref()
                    .is_some_and(|id| self.space_row(id).is_some())
            })
            .map(|c| (display_status(c, self.session_for(&c.id), now), c))
            .collect();
        sort_active(&mut rows);
        rows
    }

    /// Archived chats of a live space, newest first. The main sidebar presents
    /// these as settled sessions below the active list.
    pub fn settled_chats(&self) -> Vec<&Chat> {
        let mut rows: Vec<(ChatIndicator, &Chat)> = self
            .chats
            .iter()
            .filter(|chat| {
                chat.archived
                    && chat
                        .space_id
                        .as_deref()
                        .is_some_and(|id| self.space_row(id).is_some())
            })
            .map(|chat| (ChatIndicator::Idle, chat))
            .collect();
        sort_active(&mut rows);
        rows.into_iter().map(|(_, chat)| chat).collect()
    }

    pub fn session_for(&self, chat_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.chat_id == chat_id)
    }

    /// Staleness-checked status dot for a chat row.
    pub fn indicator_for(&self, chat_id: &str, now: DateTime<Utc>) -> Indicator {
        effective_indicator(self.session_for(chat_id), now)
    }

    pub fn selected_chat_row(&self) -> Option<&Chat> {
        let id = self.selected_chat.as_deref()?;
        self.chats.iter().find(|c| c.id == id)
    }

    pub fn gate(&self) -> GatePhase {
        gate_phase(&self.connection, self.auth.as_ref())
    }

    pub fn engine(&self) -> Option<&EngineHandle> {
        self.engine.as_ref()
    }

    // ---- gpui glue ----

    /// Kick off (or retry) the engine bootstrap: probe → connect-or-embed on
    /// tokio, then attach subscriptions. Safe to call again after `Failed`.
    pub fn bootstrap(state: Entity<AppState>, config: EngineBootConfig, cx: &mut App) {
        let data_dir = config.data_dir.clone();
        let runtime_profile = config.runtime_profile;
        let scaffold_scope = configured_scaffold_scope(&config);
        let boot_config = config.clone();
        state.update(cx, |s, cx| {
            s.connection = ConnectionStatus::Connecting;
            s.data_dir = Some(data_dir);
            s.runtime_profile = runtime_profile;
            s.scaffold_scope = scaffold_scope;
            s.boot_config = Some(boot_config);
            cx.notify();
        });
        let boot = Tokio::spawn(cx, EngineHandle::bootstrap(config));
        cx.spawn(async move |cx| {
            let outcome = match boot.await {
                Ok(Ok(handle)) => Ok(handle),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            // NB: at the pinned rev `Entity::update(&mut AsyncApp)` returns the
            // closure's value directly (no Result) — AsyncApp implements
            // AppContext like App does.
            state.update(cx, |s, cx| match outcome {
                Ok(handle) => s.attach_engine(handle, cx),
                Err(message) => {
                    tracing::error!(%message, "engine bootstrap failed");
                    s.connection = ConnectionStatus::Failed(message);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Wire the connected engine: mark Ready and start the standing watches.
    /// Methods the engine doesn't serve yet (chats/devices/auth land with the
    /// workspace doc in M4) fail their subscribe and are skipped gracefully.
    fn attach_engine(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
        self.connection = ConnectionStatus::Ready;
        self.engine = Some(handle.clone());
        self.watch_tasks = vec![
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_SESSIONS,
                AppState::apply_sessions,
            ),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_SESSION_REFS,
                AppState::apply_session_refs,
            ),
            spawn_chats_watch(cx, handle.clone()),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_DEVICES,
                AppState::apply_devices,
            ),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_SPACES,
                AppState::apply_spaces,
            ),
            // Auth frames parse tolerantly — engine and proto tags differ today.
            spawn_watch(
                cx,
                handle.clone(),
                methods::AUTH_STATUS,
                AppState::apply_auth_value,
            ),
            spawn_watch(
                cx,
                handle.clone(),
                methods::UPDATE_STATUS,
                AppState::apply_update,
            ),
            spawn_local_device_probe(cx, handle.clone()),
        ];
        // Re-subscribe selected-room projections after an engine reconnect.
        // Both retain their last good content until replacement frames arrive.
        if let Some(chat_id) = self.selected_chat.clone() {
            let projection = self.room_projections.get(&chat_id).cloned();
            self.transcript_task = Some(spawn_transcript_watch(
                cx,
                handle.clone(),
                chat_id.clone(),
                projection.clone(),
            ));
            self.collaboration_task =
                Some(spawn_collaboration_watch(cx, handle, chat_id, projection));
        }
        cx.notify();
    }

    /// A watch task saw the RPC transport die ([`RpcError::Closed`]). First
    /// caller wins: flip back to Connecting and re-run the probe-or-embed
    /// bootstrap — a restarted daemon is reattached, a vanished one is
    /// replaced by an embedded engine. Without this the app is a zombie:
    /// standing watches end silently and the transcript watch retries a dead
    /// socket every 2s forever.
    fn on_engine_closed(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.reconnect_config() else {
            return;
        };
        let old_engine = self.engine.take();
        let entity = cx.entity();
        cx.defer(move |cx| {
            if let Some(old) = old_engine {
                // Graceful for an in-process engine (releases the IPC port
                // ahead of the re-probe); a no-op for a remote daemon.
                Tokio::spawn(cx, async move { old.shutdown().await }).detach();
            }
            AppState::bootstrap(entity, config, cx);
        });
        cx.notify();
    }

    /// Guard half of the reconnect supervisor: only an attached, non-quitting
    /// app with a retained boot config reconnects. The winner flips the status
    /// to Connecting, collapsing concurrent detectors into one bootstrap.
    fn reconnect_config(&mut self) -> Option<EngineBootConfig> {
        if self.shutting_down || !matches!(self.connection, ConnectionStatus::Ready) {
            return None; // already reconnecting, failed, or quitting
        }
        let config = self.boot_config.clone()?;
        tracing::warn!("engine RPC connection closed; re-probing");
        self.connection = ConnectionStatus::Connecting;
        Some(config)
    }

    /// Async-context entry for watch tasks reporting a closed connection.
    fn engine_connection_lost(this: &WeakEntity<AppState>, cx: &mut AsyncApp) {
        this.update(cx, |state, cx| state.on_engine_closed(cx)).ok();
    }

    /// Mark the quit in progress and hand back the engine for teardown, so
    /// its closing streams can't restart a fresh engine mid-shutdown.
    pub fn begin_shutdown(&mut self) -> Option<EngineHandle> {
        self.shutting_down = true;
        self.engine.clone()
    }

    pub fn selected_scaffold_control_grant_id(&self) -> Option<&str> {
        self.selected_chat
            .as_deref()
            .and_then(|chat_id| self.scaffold_control_grants.get(chat_id))
            .map(String::as_str)
    }
    pub(crate) fn scaffold_control_target(&self, chat_id: &str) -> Option<&ScaffoldControlTarget> {
        self.scaffold_control_targets.get(chat_id)
    }

    pub fn selected_chat_is_scaffold_room(&self) -> bool {
        self.selected_chat
            .as_ref()
            .is_some_and(|chat_id| self.room_projections.contains_key(chat_id))
    }
    pub fn chat_is_scaffold(&self, chat_id: &str) -> bool {
        self.pending_scaffold_session
            .as_ref()
            .is_some_and(|draft| draft.chat_id == chat_id)
            || self.room_projections.contains_key(chat_id)
    }

    pub fn can_start_scaffold_session(&self) -> bool {
        !self.scaffold_session_creating
            && self.scaffold_scope.is_some()
            && self.selected_space.is_some()
            && self.runtime_profile.allows_session_import()
    }

    pub fn scaffold_session_creating(&self) -> bool {
        self.scaffold_session_creating
    }
    fn select_pending_scaffold_chat(
        &mut self,
        draft: ScaffoldSessionDraft,
        cx: &mut Context<Self>,
    ) {
        let chat_id = draft.chat_id.clone();
        self.pending_scaffold_session = Some(draft);
        self.mark_chat_pending(&chat_id);
        // `select_chat` keeps pending Scaffold drafts unopened until Attach
        // returns their exact room projection.
        self.select_chat(Some(chat_id), cx);
    }

    /// Create one local Comet session under the selected folder. No Scaffold
    /// environment RPC is made until this exact session sends its first prompt.
    pub fn start_scaffold_session(&mut self, cx: &mut Context<Self>) {
        if self.scaffold_session_creating {
            return;
        }
        let Some((project_id, deployment_id)) = self.scaffold_scope.clone() else {
            self.scaffold_session_error = Some("Scaffold is not configured".into());
            cx.notify();
            return;
        };
        let Some(space_id) = self.selected_space.clone() else {
            self.scaffold_session_error = Some("Select a folder first".into());
            cx.notify();
            return;
        };
        let Some(handle) = self.engine.clone() else {
            self.scaffold_session_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let chat_id = uuid::Uuid::new_v4().to_string();
        self.scaffold_session_creating = true;
        self.scaffold_session_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = handle
                .client()
                .call(
                    methods::MUTATE,
                    serde_json::json!({
                        "op": "createChat",
                        "chatId": chat_id,
                        "spaceId": space_id,
                    }),
                )
                .await;
            let _ = this.update(cx, |state, cx| {
                state.scaffold_session_creating = false;
                match result {
                    Ok(_) => state.select_pending_scaffold_chat(
                        ScaffoldSessionDraft {
                            project_id,
                            deployment_id,
                            space_id,
                            chat_id,
                        },
                        cx,
                    ),
                    Err(err) => {
                        tracing::warn!(error = %err, "Scaffold Comet session creation failed");
                        state.scaffold_session_error =
                            Some("Could not create Scaffold session".into());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub(crate) fn scaffold_session_draft(&self) -> Option<&ScaffoldSessionDraft> {
        self.pending_scaffold_session.as_ref()
    }

    pub(crate) fn install_scaffold_session(
        &mut self,
        attachment: &ScaffoldSessionAttachment,
        cx: &mut Context<Self>,
    ) {
        let chat_id = attachment.projection.session_id.clone();
        self.room_projections
            .insert(chat_id.clone(), attachment.projection.clone());
        self.scaffold_control_grants
            .insert(chat_id.clone(), attachment.grant_id.clone());
        self.scaffold_control_targets
            .insert(chat_id.clone(), attachment.control_target.clone());
        self.pending_scaffold_session = None;
        self.scaffold_session_error = None;
        if self.selected_chat.as_deref() != Some(chat_id.as_str()) {
            self.select_chat(Some(chat_id), cx);
            return;
        }
        self.transcript.clear();
        self.transcript_task = None;
        self.collaboration = None;
        self.collaboration_task = None;
        self.selected_agent_session = None;
        self.selected_invitation_grant = None;
        if let Some(handle) = self.engine.clone() {
            let projection = Some(attachment.projection.clone());
            self.transcript_task = Some(spawn_transcript_watch(
                cx,
                handle.clone(),
                chat_id.clone(),
                projection.clone(),
            ));
            self.collaboration_task =
                Some(spawn_collaboration_watch(cx, handle, chat_id, projection));
        }
        cx.notify();
    }

    pub fn mark_scaffold_chat_starting(&mut self, chat_id: &str) {
        self.scaffold_starting_chats.insert(chat_id.to_string());
    }

    pub fn clear_scaffold_chat_starting(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        if !self.scaffold_starting_chats.remove(chat_id)
            || self.selected_chat.as_deref() != Some(chat_id)
            || self.transcript_task.is_some()
            || self.collaboration_task.is_some()
        {
            return;
        }
        let Some(handle) = self.engine.clone() else {
            return;
        };
        let projection = self.room_projections.get(chat_id).cloned();
        self.transcript_task = Some(spawn_transcript_watch(
            cx,
            handle.clone(),
            chat_id.to_string(),
            projection.clone(),
        ));
        self.collaboration_task = Some(spawn_collaboration_watch(
            cx,
            handle,
            chat_id.to_string(),
            projection,
        ));
        cx.notify();
    }

    pub fn scaffold_chat_starting(&self, chat_id: &str) -> bool {
        self.scaffold_starting_chats.contains(chat_id)
    }

    pub fn load_local_sessions(&mut self, force: bool, cx: &mut Context<Self>) {
        if !self.runtime_profile.allows_session_import() {
            return;
        }
        if self.local_sessions_loading
            || (!force
                && self
                    .local_sessions_refreshed_at
                    .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(30)))
        {
            return;
        }
        let Some(handle) = self.engine.clone() else {
            return;
        };
        self.local_sessions_loading = true;
        self.local_sessions_refreshed_at = Some(std::time::Instant::now());
        self.local_sessions_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = handle
                .client()
                .call(methods::LIST_LOCAL_SESSIONS, serde_json::json!({}))
                .await
                .and_then(|value| {
                    serde_json::from_value::<Vec<LocalSessionCandidate>>(value)
                        .map_err(|err| RpcError::Failed(err.to_string()))
                });
            let _ = this.update(cx, |state, cx| {
                state.local_sessions_loading = false;
                match result {
                    Ok(candidates) => state.apply_local_session_candidates(candidates),
                    Err(err) => {
                        tracing::warn!(error = %err, "local session discovery failed");
                        state.local_sessions_error =
                            Some(format!("Could not find local sessions: {err}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn attach_local_session(&mut self, candidate_id: String, cx: &mut Context<Self>) {
        if self.local_session_attaching.contains(&candidate_id)
            || !self
                .local_session_candidates
                .iter()
                .any(|candidate| candidate.id == candidate_id)
        {
            return;
        }
        let failure_verb = self
            .local_session_candidates
            .iter()
            .find(|candidate| candidate.id == candidate_id)
            .map(|candidate| {
                if candidate.history_only {
                    "import"
                } else {
                    "open"
                }
            })
            .unwrap_or("open");
        let Some(handle) = self.engine.clone() else {
            return;
        };
        self.local_session_attaching.insert(candidate_id.clone());
        self.local_session_attach_errors.remove(&candidate_id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = handle
                .client()
                .call(
                    methods::ATTACH_LOCAL_SESSION,
                    serde_json::json!({ "candidateId": candidate_id.clone() }),
                )
                .await
                .and_then(|value| {
                    serde_json::from_value::<LocalSessionAttachResult>(value)
                        .map_err(|err| RpcError::Failed(err.to_string()))
                });
            let _ = this.update(cx, |state, cx| {
                state.local_session_attaching.remove(&candidate_id);
                match result {
                    Ok(attached) => {
                        state
                            .local_session_candidates
                            .retain(|candidate| candidate.id != candidate_id);
                        state.local_session_attach_errors.remove(&candidate_id);
                        let chat_id = attached.chat_id;
                        if !state.chats.iter().any(|chat| chat.id == chat_id) {
                            state.pending_local_chat_ids.insert(chat_id.clone());
                        }
                        state.selected_space = Some(attached.space_id);
                        state.select_chat(Some(chat_id), cx);
                    }
                    Err(err) => {
                        tracing::warn!(
                            %candidate_id,
                            error = %err,
                            "local session attach failed"
                        );
                        state.local_session_attach_errors.insert(
                            candidate_id.clone(),
                            format!("Could not {failure_verb}: {err}"),
                        );
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Select a chat (or clear). Swaps the per-chat doc-transcript subscription:
    /// dropping the old task drops its stream receiver, which cancels the doc
    /// watch server-side. Selecting a chat also lands in its space and marks it
    /// seen (a global-list click must switch the tab strip too).
    pub fn select_chat(&mut self, chat_id: Option<String>, cx: &mut Context<Self>) {
        if self
            .pending_scaffold_session
            .as_ref()
            .is_some_and(|draft| Some(draft.chat_id.as_str()) != chat_id.as_deref())
        {
            self.pending_scaffold_session = None;
        }
        if self.selected_chat == chat_id {
            // An explicit click on an already-open new-session canvas must
            // still suppress the boot watch's automatic prior-chat selection.
            self.auto_selected = true;
            // Re-selecting still clears a fresh "completed" badge.
            if let Some(id) = chat_id {
                self.mark_chat_seen(&id, cx);
            }
            return;
        }
        self.selected_chat = chat_id.clone();
        self.auto_selected = true;
        self.transcript.clear();
        self.transcript_task = None;
        self.collaboration = None;
        self.collaboration_task = None;
        self.selected_agent_session = None;
        self.selected_invitation_grant = None;
        if self
            .pending_invitation
            .as_ref()
            .is_some_and(|invitation| Some(invitation.chat_id.as_str()) != chat_id.as_deref())
        {
            self.pending_invitation = None;
        }
        if let Some(id) = chat_id.as_deref() {
            // A chat implies its space; `select_chat(None)` (the new-session
            // canvas) stays within the current space.
            if let Some(space_id) = self
                .chats
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.space_id.clone())
            {
                if !self.selected_space_members.contains(&space_id) {
                    self.selected_space_members = vec![space_id.clone()];
                }
                self.selected_space = Some(space_id);
            }
            self.mark_chat_seen(id, cx);
        }
        if let (Some(chat_id), Some(handle)) = (chat_id, self.engine.clone())
            && !self.scaffold_starting_chats.contains(&chat_id)
            && !self
                .pending_scaffold_session
                .as_ref()
                .is_some_and(|draft| draft.chat_id == chat_id)
        {
            let projection = self.room_projections.get(&chat_id).cloned();
            self.transcript_task = Some(spawn_transcript_watch(
                cx,
                handle.clone(),
                chat_id.clone(),
                projection.clone(),
            ));
            self.collaboration_task =
                Some(spawn_collaboration_watch(cx, handle, chat_id, projection));
        }
        cx.notify();
    }

    /// Select one persisted space; the caller decides which chat to land on.
    pub fn select_space(&mut self, space_id: Option<String>, cx: &mut Context<Self>) {
        match space_id {
            Some(space_id) => self.select_space_source(space_id.clone(), vec![space_id], cx),
            None => {
                if self.selected_space.is_none() && self.selected_space_members.is_empty() {
                    return;
                }
                self.selected_space = None;
                self.selected_space_members.clear();
                self.pending_scaffold_session = None;
                cx.notify();
            }
        }
    }

    /// Select a presentation-only logical source while retaining every
    /// persisted member id for tab/chat projection.
    pub fn select_space_source(
        &mut self,
        space_id: String,
        mut member_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if !member_ids.contains(&space_id) {
            member_ids.push(space_id.clone());
        }
        member_ids.sort();
        member_ids.dedup();
        if self.selected_space.as_deref() == Some(space_id.as_str())
            && self.selected_space_members == member_ids
        {
            return;
        }
        self.pending_scaffold_session = None;
        self.selected_space = Some(space_id);
        self.selected_space_members = member_ids;
        cx.notify();
    }

    /// Window-focus liveness sweep for every open room.
    pub fn probe_sync(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.engine.clone() else {
            return;
        };
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({});
            if let Err(err) = handle.client().call(methods::PROBE_SYNC, params).await {
                tracing::debug!(error = %err, "probe sync failed");
            }
        })
        .detach();
    }

    /// Synced seen marker: only fires when the chat is currently unseen
    /// (idempotence — no mutate spam), stamps the local row optimistically so
    /// the LWW round-trip is invisible, and fire-and-forgets the mutate.
    pub fn mark_chat_seen(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        // An explicit read releases the "marked unread" pin either way.
        self.unread_marks.remove(chat_id);
        let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) else {
            return;
        };
        if !chat.unseen() {
            return;
        }
        chat.last_seen_at = Some(Utc::now());
        cx.notify();
        let Some(handle) = self.engine.clone() else {
            return;
        };
        let chat_id = chat_id.to_string();
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({ "op": "markChatSeen", "chatId": chat_id });
            if let Err(err) = handle.client().call(methods::MUTATE, params).await {
                tracing::warn!(chat = %chat_id, error = %err, "markChatSeen failed");
            }
        })
        .detach();
    }

    /// Explicit "mark as unread": clears the synced seen marker (optimistically
    /// locally, LWW mutate to every device) and pins the chat against the
    /// shell's window-active auto-seen stamp until the user re-selects it.
    pub fn mark_chat_unread(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) else {
            return;
        };
        chat.last_seen_at = None;
        self.unread_marks.insert(chat_id.to_string());
        cx.notify();
        let Some(handle) = self.engine.clone() else {
            return;
        };
        let chat_id = chat_id.to_string();
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({ "op": "markChatUnread", "chatId": chat_id });
            if let Err(err) = handle.client().call(methods::MUTATE, params).await {
                tracing::warn!(chat = %chat_id, error = %err, "markChatUnread failed");
            }
        })
        .detach();
    }

    /// True while an explicit "mark unread" is pinned (the shell's
    /// looking-at-it auto-seen stamp must not clear it).
    pub fn chat_marked_unread(&self, chat_id: &str) -> bool {
        self.unread_marks.contains(chat_id)
    }
}

/// Subscribe to a watch method and pump each frame through `apply`. Runs on the
/// gpui executor; ends when the stream closes or the entity is released.
/// Chats watch with boot auto-select: comet's `/` route redirected to the
/// last-used chat; we approximate by selecting the most recent unarchived chat
/// on the first frame when nothing is selected yet (manual selection wins).
fn spawn_chats_watch(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = match handle
            .client()
            .subscribe(methods::WATCH_CHATS, serde_json::json!({}))
            .await
        {
            Ok(rx) => rx,
            Err(RpcError::Closed) => {
                AppState::engine_connection_lost(&this, cx);
                return;
            }
            Err(err) => {
                tracing::debug!(error = %err, "chats watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let parsed: Vec<Chat> = match serde_json::from_value(value) {
                Ok(parsed) => parsed,
                Err(err) => {
                    tracing::warn!(error = %err, "dropping malformed chats frame");
                    continue;
                }
            };
            let alive = this.update(cx, |state, cx| {
                state.apply_chats(parsed);
                if state.selected_chat.is_none() && !state.auto_selected {
                    let most_recent = state
                        .chats
                        .iter()
                        .find(|c| !c.archived)
                        .map(|c| c.id.clone());
                    if let Some(chat_id) = most_recent {
                        state.auto_selected = true;
                        state.select_chat(Some(chat_id), cx);
                    }
                }
                cx.notify();
            });
            if alive.is_err() {
                return;
            }
        }
        // Stream ended with the entity alive: dead transport, or a
        // server-side end (an engine that doesn't serve the method replies
        // with an err frame that just closes the stream). Only a dead
        // transport reconnects — any unary reply, even "unknown method",
        // proves the connection is still alive.
        if matches!(
            handle
                .client()
                .call(methods::LOCAL_DEVICE, serde_json::json!({}))
                .await,
            Err(RpcError::Closed)
        ) {
            AppState::engine_connection_lost(&this, cx);
        }
    })
}

fn spawn_watch<T: DeserializeOwned + 'static>(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    method: &'static str,
    apply: fn(&mut AppState, T),
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = match handle
            .client()
            .subscribe(method, serde_json::json!({}))
            .await
        {
            Ok(rx) => rx,
            Err(RpcError::Closed) => {
                AppState::engine_connection_lost(&this, cx);
                return;
            }
            Err(err) => {
                tracing::debug!(method, error = %err, "watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let parsed: T = match serde_json::from_value(value) {
                Ok(parsed) => parsed,
                Err(err) => {
                    tracing::warn!(method, error = %err, "dropping malformed watch frame");
                    continue;
                }
            };
            let alive = this.update(cx, |state, cx| {
                apply(state, parsed);
                cx.notify();
            });
            if alive.is_err() {
                return;
            }
        }
        // See spawn_chats_watch: reconnect only on a provably dead transport.
        if matches!(
            handle
                .client()
                .call(methods::LOCAL_DEVICE, serde_json::json!({}))
                .await,
            Err(RpcError::Closed)
        ) {
            AppState::engine_connection_lost(&this, cx);
        }
    })
}

/// Best-effort `LocalDevice` probe: fills `local_device_id` for the "This
/// device" badge. Engines that don't serve the method leave it `None`.
fn spawn_local_device_probe(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let Ok(value) = handle
            .client()
            .call("LocalDevice", serde_json::json!({}))
            .await
        else {
            tracing::debug!("LocalDevice unavailable; skipping this-device badge");
            return;
        };
        let id = value
            .get("id")
            .or_else(|| value.get("deviceId"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(id) = id {
            this.update(cx, |state, cx| {
                state.local_device_id = Some(id);
                cx.notify();
            })
            .ok();
        }
    })
}

fn spawn_transcript_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    chat_id: String,
    room_projection: Option<SessionRoomProjection>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Outer loop: a delta desync (missed frame) resubscribes immediately
        // and the fresh stream's opening reset heals the copy; a subscribe
        // failure, malformed frame, or stream end retries on a delay. Every
        // path re-enters the loop except a dead transport, which hands off to
        // the reconnect supervisor (attach_engine re-spawns this watch for the
        // selected chat once the engine is back). The task itself is dropped
        // by select_chat/apply_chats when the chat is deselected or deleted,
        // so retrying can't outlive relevance.
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        'resubscribe: loop {
            let params = serde_json::json!({
                "chatId": chat_id,
                "roomProjection": room_projection,
            });
            let mut rx = match handle
                .client()
                .subscribe(methods::WATCH_DOC_MESSAGES, params)
                .await
            {
                Ok(rx) => rx,
                Err(RpcError::Closed) => {
                    // No resubscribe can heal a dead transport; without the
                    // handoff this arm retried every 2s for hours (~8k warns
                    // per zombie session in the field).
                    AppState::engine_connection_lost(&this, cx);
                    return;
                }
                Err(err) => {
                    tracing::warn!(%chat_id, error = %err, "transcript watch failed; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue 'resubscribe;
                }
            };
            while let Some(value) = rx.recv().await {
                let frame: TranscriptFrame = match serde_json::from_value(value) {
                    Ok(frame) => frame,
                    Err(err) => {
                        // Schema skew (a newer peer's entry shape arriving
                        // through sync): a skipped frame is a silently stale
                        // copy, so resubscribe for a fresh reset — delayed,
                        // in case the reset itself is what can't parse.
                        tracing::warn!(error = %err, "malformed transcript frame; resubscribing");
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue 'resubscribe;
                    }
                };
                let mut desync = false;
                let alive = this.update(cx, |state, cx| {
                    // Guard against a stale pump racing a newer selection.
                    if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                        if let Err(err) = state.apply_transcript_frame(frame) {
                            tracing::warn!(%chat_id, error = %err, "resubscribing transcript");
                            desync = true;
                        }
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
                if desync {
                    continue 'resubscribe;
                }
            }
            // Stream ended: engine restart, RPC drop, or chat purge. Retry;
            // the purge case is cleaned up by apply_chats dropping this task.
            tracing::debug!(%chat_id, "transcript stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}
fn spawn_collaboration_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    chat_id: String,
    room_projection: Option<SessionRoomProjection>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let params = serde_json::json!({
                "chatId": chat_id,
                "roomProjection": room_projection,
            });
            let mut rx = match handle
                .client()
                .subscribe("WatchCollaboration", params)
                .await
            {
                Ok(rx) => rx,
                Err(RpcError::Closed) => {
                    // Dead transport — hand off to the reconnect supervisor;
                    // attach_engine re-spawns this watch after reconnect.
                    AppState::engine_connection_lost(&this, cx);
                    return;
                }
                Err(err) => {
                    tracing::warn!(%chat_id, error = %err, "collaboration watch failed; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let snapshot: CollaborationSnapshot = match serde_json::from_value(value) {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        tracing::warn!(%chat_id, error = %err, "malformed collaboration snapshot");
                        continue;
                    }
                };
                if this
                    .update(cx, |state, cx| {
                        if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                            state.apply_collaboration(snapshot);
                            state.drain_session_pin(cx);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
            // Keep the last good snapshot on screen while the room reconnects.
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use comet_engine::{EngineCore, default_registry};
    use gpui::AppContext;
    // `SessionStatus` is only needed to build the fixtures below — the module
    // itself derives everything through `comet_proto::view`.
    use comet_proto::{SessionStatus, UserProfile};
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicU16, Ordering},
    };

    struct ReadyScaffoldRpc {
        operations: Arc<StdMutex<Vec<String>>>,
        attach_failures_remaining: AtomicU16,
        inspect_lifecycle: &'static str,
        archive_fails: bool,
    }

    #[async_trait::async_trait]
    impl RpcService for ReadyScaffoldRpc {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            if method == methods::MUTATE {
                assert_eq!(
                    params,
                    serde_json::json!({
                        "op": "setChatArchived",
                        "chatId": "session-ready",
                        "archived": true,
                    })
                );
                self.operations
                    .lock()
                    .expect("Scaffold operation log")
                    .push("archive".into());
                if self.archive_fails {
                    return Err(RpcError::Failed("archive failed".into()));
                }
                return RpcReply::value(&serde_json::json!({ "ok": true }));
            }
            assert_eq!(method, methods::CONTROL_SCAFFOLD_ENVIRONMENT);
            let operation = params
                .get("operation")
                .and_then(serde_json::Value::as_str)
                .expect("typed Scaffold operation");
            if operation == "create" {
                assert_eq!(
                    params.get("source_ref").and_then(serde_json::Value::as_str),
                    Some("feat/comet-identity-integration")
                );
                assert_eq!(
                    params.get("agentRoute"),
                    Some(&serde_json::json!({
                        "provider": "openai",
                        "model": "gpt-5.6-sol",
                        "fallback": "disabled",
                        "routingMode": "automatic",
                    }))
                );
            }
            self.operations
                .lock()
                .expect("Scaffold operation log")
                .push(operation.to_string());
            if operation == "attach"
                && self
                    .attach_failures_remaining
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
            {
                return Err(RpcError::Failed(
                    "scaffold_api_error:502:scaffold_request_rejected".into(),
                ));
            }
            let scope = params.get("scope").expect("Scaffold scope");
            let session_id = scope
                .get("sessionId")
                .and_then(serde_json::Value::as_str)
                .expect("session id");
            let lifecycle = match operation {
                "inspect" => self.inspect_lifecycle,
                "pause" => "paused",
                _ => "starting",
            };
            let environment = serde_json::json!({
                "source": {
                    "kind": "scaffold",
                    "sandbox_id": "sandbox-ready",
                    "region": "default",
                    "lifecycle": lifecycle,
                    "lifecycle_epoch": 1,
                    "links": {}
                },
                "ownerPrincipal": "accounts.google.com:ready@example.com",
                "sourceRef": "387d6652abd642f0b85e8bd14f9131a9f23b7e70",
                "scope": {
                    "projectId": "ashler-staging",
                    "deploymentId": "ashler-staging",
                    "sessionId": session_id
                },
                "lastActivityAt": 1,
            });
            let result = if operation == "attach" {
                serde_json::json!({
                    "environment": environment,
                    "attachedDeviceId": "comet-scaffold-sandbox-ready-e1",
                    "runId": "run-ready",
                    "roomProjection": {
                        "projectId": "ashler-staging",
                        "deploymentId": "ashler-staging",
                        "sessionId": session_id
                    },
                    "controlGrant": {
                        "id": "grant-ready",
                        "expiresAt": 9_999_999_999_999_i64,
                        "capabilities": [comet_proto::CAPABILITY_SESSION_CHAT]
                    }
                })
            } else {
                assert!(matches!(
                    operation,
                    "create" | "inspect" | "pause" | "resume"
                ));
                serde_json::json!({ "environment": environment })
            };
            RpcReply::value(&result)
        }
    }

    /// A localhost port that was just free — picked OUTSIDE the OS ephemeral
    /// range (macOS: 49152+), which `:0` allocations and outbound sockets in
    /// parallel test processes draw from. Binding `:0` and re-using the port
    /// after drop raced those allocations in the free→bootstrap window.
    static NEXT_TEST_PORT_OFFSET: AtomicU16 = AtomicU16::new(0);
    async fn free_port() -> u16 {
        let start = 20000 + (std::process::id() % 10000) as u16;
        for _ in 0..2000 {
            let offset = NEXT_TEST_PORT_OFFSET.fetch_add(1, Ordering::Relaxed);
            let port = start + (offset % 2000);
            if tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return port;
            }
        }
        panic!("no free port in 20000-32000");
    }

    #[test]
    fn local_scaffold_scope_defaults_deployment_to_project() {
        let mut config = EngineBootConfig {
            data_dir: PathBuf::new(),
            ipc_port: 0,
            edge_url: "https://edge.example".into(),
            edge_token: None,
            project_scope: "ashler-staging".into(),
            deployment_id: None,
            scaffold_url: Some("https://scaffold.example".into()),
            default_harness: HarnessId::Omp,
            runtime_profile: RuntimeProfile::LocalController,
        };
        assert_eq!(
            configured_scaffold_scope(&config),
            Some(("ashler-staging".into(), "ashler-staging".into()))
        );
        config.deployment_id = Some("deployment-a".into());
        assert_eq!(
            configured_scaffold_scope(&config),
            Some(("ashler-staging".into(), "deployment-a".into()))
        );
        config.scaffold_url = None;
        assert_eq!(configured_scaffold_scope(&config), None);
    }

    #[tokio::test]
    async fn scaffold_attaches_while_starting_before_readiness_inspection() {
        let operations = Arc::new(StdMutex::new(Vec::new()));
        let service: Arc<dyn RpcService> = Arc::new(ReadyScaffoldRpc {
            operations: Arc::clone(&operations),
            attach_failures_remaining: AtomicU16::new(0),
            inspect_lifecycle: "ready",
            archive_fails: false,
        });
        let handle = EngineHandle {
            inner: Arc::new(RemoteEngine {
                client: memory_client(service),
                url: "memory://ready-scaffold".into(),
            }),
        };
        let scope = CollaborationScope {
            project_id: "ashler-staging".into(),
            deployment_id: Some("ashler-staging".into()),
            session_id: Some("session-ready".into()),
            unknown: Default::default(),
        };
        let no_wait = |_| futures::future::ready(());

        let (sandbox_id, attachment) = create_and_attach_scaffold_session(
            &handle,
            &scope,
            Some("feat/comet-identity-integration"),
            &AgentRoute::automatic(comet_proto::AgentProvider::OpenAi, "gpt-5.6-sol"),
            &no_wait,
        )
        .await
        .unwrap();
        assert_eq!(sandbox_id, "sandbox-ready");
        assert_eq!(
            operations
                .lock()
                .expect("Scaffold operation log")
                .as_slice(),
            ["create", "attach"]
        );
        assert_eq!(
            inspect_scaffold_session(&handle, &sandbox_id, &scope)
                .await
                .unwrap(),
            ScaffoldLifecycle::Ready
        );
        assert_eq!(
            operations
                .lock()
                .expect("Scaffold operation log")
                .as_slice(),
            ["create", "attach", "inspect"]
        );
        assert_eq!(attachment.projection.session_id, "session-ready");
        assert_eq!(
            attachment.owner_device_id,
            "comet-scaffold-sandbox-ready-e1"
        );
        assert_eq!(attachment.grant_id, "grant-ready");
        assert_eq!(
            attachment.actor_subject,
            "accounts.google.com:ready@example.com"
        );
        assert_eq!(
            attachment.source_ref.as_deref(),
            Some("387d6652abd642f0b85e8bd14f9131a9f23b7e70")
        );
        assert_eq!(
            attachment.control_target,
            ScaffoldControlTarget {
                sandbox_id: "sandbox-ready".into(),
                scope: scope.clone(),
            }
        );
        archive_and_pause_scaffold_session(&handle, "session-ready", &attachment.control_target)
            .await
            .unwrap();
        assert_eq!(
            operations
                .lock()
                .expect("Scaffold operation log")
                .as_slice(),
            ["create", "attach", "inspect", "archive", "pause"]
        );
    }

    #[test]
    fn scaffold_attach_retries_transient_provider_claim_conflicts() {
        assert!(is_retryable_scaffold_attach_error(&RpcError::Failed(
            "scaffold_api_error:502:scaffold_request_rejected".into()
        )));
        assert!(!is_retryable_scaffold_attach_error(&RpcError::Failed(
            "scaffold_response_invalid: sandbox failed".into()
        )));

        let runtime = tokio::runtime::Runtime::new().expect("Tokio RPC server runtime");
        let (handle, operations) = {
            let _runtime_guard = runtime.enter();
            let operations = Arc::new(StdMutex::new(Vec::new()));
            let service: Arc<dyn RpcService> = Arc::new(ReadyScaffoldRpc {
                operations: Arc::clone(&operations),
                attach_failures_remaining: AtomicU16::new(2),
                inspect_lifecycle: "ready",
                archive_fails: false,
            });
            let handle = EngineHandle {
                inner: Arc::new(RemoteEngine {
                    client: memory_client(service),
                    url: "memory://transient-scaffold".into(),
                }),
            };
            (handle, operations)
        };

        futures::executor::block_on(async {
            let scope = CollaborationScope {
                project_id: "ashler-staging".into(),
                deployment_id: Some("ashler-staging".into()),
                session_id: Some("session-transient".into()),
                unknown: Default::default(),
            };
            let no_wait = |_| futures::future::ready(());
            let (sandbox_id, attachment) = create_and_attach_scaffold_session(
                &handle,
                &scope,
                Some("feat/comet-identity-integration"),
                &AgentRoute::automatic(comet_proto::AgentProvider::OpenAi, "gpt-5.6-sol"),
                &no_wait,
            )
            .await
            .unwrap();

            assert_eq!(sandbox_id, "sandbox-ready");
            assert_eq!(attachment.projection.session_id, "session-transient");
            assert_eq!(
                operations
                    .lock()
                    .expect("Scaffold operation log")
                    .as_slice(),
                ["create", "attach", "attach", "attach"]
            );
        });
    }

    #[tokio::test]
    async fn failed_archive_does_not_pause_scaffold() {
        let operations = Arc::new(StdMutex::new(Vec::new()));
        let service: Arc<dyn RpcService> = Arc::new(ReadyScaffoldRpc {
            operations: Arc::clone(&operations),
            attach_failures_remaining: AtomicU16::new(0),
            inspect_lifecycle: "ready",
            archive_fails: true,
        });
        let handle = EngineHandle {
            inner: Arc::new(RemoteEngine {
                client: memory_client(service),
                url: "memory://failed-archive".into(),
            }),
        };
        let target = ScaffoldControlTarget {
            sandbox_id: "sandbox-ready".into(),
            scope: CollaborationScope {
                project_id: "ashler-staging".into(),
                deployment_id: Some("ashler-staging".into()),
                session_id: Some("session-ready".into()),
                unknown: Default::default(),
            },
        };

        assert!(
            archive_and_pause_scaffold_session(&handle, "session-ready", &target)
                .await
                .is_err()
        );
        assert_eq!(
            operations
                .lock()
                .expect("Scaffold operation log")
                .as_slice(),
            ["archive"]
        );
    }

    #[tokio::test]
    async fn paused_scaffold_resumes_before_reattaching() {
        let operations = Arc::new(StdMutex::new(Vec::new()));
        let service: Arc<dyn RpcService> = Arc::new(ReadyScaffoldRpc {
            operations: Arc::clone(&operations),
            attach_failures_remaining: AtomicU16::new(0),
            inspect_lifecycle: "paused",
            archive_fails: false,
        });
        let handle = EngineHandle {
            inner: Arc::new(RemoteEngine {
                client: memory_client(service),
                url: "memory://paused-scaffold".into(),
            }),
        };
        let scope = CollaborationScope {
            project_id: "ashler-staging".into(),
            deployment_id: Some("ashler-staging".into()),
            session_id: Some("session-paused".into()),
            unknown: Default::default(),
        };
        let no_wait = |_| futures::future::ready(());

        let attachment =
            ensure_scaffold_session_attached(&handle, "sandbox-ready", &scope, &no_wait)
                .await
                .unwrap();

        assert_eq!(attachment.projection.session_id, "session-paused");
        assert_eq!(
            operations
                .lock()
                .expect("Scaffold operation log")
                .as_slice(),
            ["inspect", "resume", "attach"]
        );
    }

    #[tokio::test]
    async fn bootstrap_embeds_engine_when_port_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            project_scope: "test".into(),
            deployment_id: None,
            scaffold_url: None,
            default_harness: HarnessId::Mock,
            runtime_profile: RuntimeProfile::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);
        // Same protocol over the in-memory transport: a real engine answers.
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn an_embedded_engine_serves_the_ipc_port_for_other_viewports() {
        // The whole point of embedding-and-serving: a second viewport (the
        // terminal app) can attach to this window's engine with no setup, no
        // separate daemon, and no launch ordering.
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().await;
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            project_scope: "test".into(),
            deployment_id: None,
            scaffold_url: None,
            default_harness: HarnessId::Mock,
            runtime_profile: RuntimeProfile::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);

        // Attach the way an external viewport would, and speak the same protocol.
        let attached = connect_ws(&format!("ws://127.0.0.1:{port}"))
            .await
            .expect("a second viewport must be able to attach");
        let harnesses = attached
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));

        // Shutting the window down stops accepting, so the next viewport
        // starts its own engine rather than talking to closing stores.
        handle.shutdown().await;
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "the port must be released on shutdown"
        );
    }

    #[tokio::test]
    async fn a_stranger_on_the_ipc_port_does_not_wedge_the_window() {
        // The port probe only proves *something* is listening. A process that
        // accepts TCP and never speaks WebSocket used to hang the dial forever;
        // now it times out and we embed instead, losing only the ability to
        // serve other viewports.
        let squatter = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = squatter.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            project_scope: "test".into(),
            deployment_id: None,
            scaffold_url: None,
            default_harness: HarnessId::Mock,
            runtime_profile: RuntimeProfile::Mock,
        })
        .await
        .expect("a taken port must not fail the boot");
        assert_eq!(handle.mode(), EngineMode::InProcess);
        assert!(
            handle
                .client()
                .call(methods::LIST_HARNESSES, serde_json::json!({}))
                .await
                .is_ok(),
            "the window still works over its own transport"
        );
        handle.shutdown().await;
        drop(squatter);
    }

    #[tokio::test]
    async fn bootstrap_connects_when_daemon_is_listening() {
        // Stand in for `comet headless`: an engine served over the WS IPC port.
        let daemon_dir = tempfile::tempdir().unwrap();
        let core = EngineCore::assemble(
            daemon_dir.path(),
            Arc::new(default_registry(
                comet_proto::RuntimeProfile::LocalController,
            )),
            HarnessId::Mock,
            None,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(comet_rpc::serve_ws_listener(listener, core.rpc_service()));

        let ui_dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: ui_dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            project_scope: "test".into(),
            deployment_id: None,
            scaffold_url: None,
            default_harness: HarnessId::Mock,
            runtime_profile: RuntimeProfile::Mock,
        })
        .await
        .unwrap();
        assert_eq!(
            handle.mode(),
            EngineMode::Remote {
                url: format!("ws://127.0.0.1:{port}")
            }
        );
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
    }

    fn chat(id: &str, created_min: i64, last_msg_min: Option<i64>) -> Chat {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: last_msg_min.map(|m| base + TimeDelta::minutes(m)),
            created_at: base + TimeDelta::minutes(created_min),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
        }
    }

    #[test]
    fn scaffold_draft_preserves_exact_first_prompt_scope() {
        let draft = ScaffoldSessionDraft {
            project_id: "project-a".into(),
            deployment_id: "deployment-a".into(),
            space_id: "space-a".into(),
            chat_id: "chat-a".into(),
        };

        let scope = draft.collaboration_scope();

        assert_eq!(scope.project_id, "project-a");
        assert_eq!(scope.deployment_id.as_deref(), Some("deployment-a"));
        assert_eq!(scope.session_id.as_deref(), Some("chat-a"));
    }

    #[gpui::test]
    fn scaffold_draft_is_bound_to_its_selected_comet_session(cx: &mut gpui::TestAppContext) {
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, cx| {
            state.pending_scaffold_session = Some(ScaffoldSessionDraft {
                project_id: "project-a".into(),
                deployment_id: "deployment-a".into(),
                space_id: "space-a".into(),
                chat_id: "chat-a".into(),
            });

            state.select_chat(Some("chat-a".into()), cx);
            assert!(state.chat_is_scaffold("chat-a"));

            state.select_chat(None, cx);
            assert!(state.pending_scaffold_session.is_none());
        });
    }
    #[gpui::test]
    fn pending_scaffold_chat_does_not_open_the_unscoped_room(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Runtime::new().expect("Tokio test runtime");
        let _runtime_guard = runtime.enter();
        let operations = Arc::new(StdMutex::new(Vec::new()));
        let service: Arc<dyn RpcService> = Arc::new(ReadyScaffoldRpc {
            operations: Arc::clone(&operations),
            attach_failures_remaining: AtomicU16::new(0),
            inspect_lifecycle: "ready",
            archive_fails: false,
        });
        let handle = EngineHandle {
            inner: Arc::new(RemoteEngine {
                client: memory_client(service),
                url: "memory://pending-scaffold".into(),
            }),
        };
        let state = cx.new(|_| AppState::new());

        state.update(cx, |state, cx| {
            state.engine = Some(handle);
            state.select_pending_scaffold_chat(
                ScaffoldSessionDraft {
                    project_id: "project-a".into(),
                    deployment_id: "deployment-a".into(),
                    space_id: "space-a".into(),
                    chat_id: "chat-a".into(),
                },
                cx,
            );

            assert_eq!(state.selected_chat.as_deref(), Some("chat-a"));
            assert!(!state.scaffold_chat_starting("chat-a"));
            assert!(state.transcript_task.is_none());
            assert!(state.collaboration_task.is_none());
        });
        assert!(
            operations
                .lock()
                .expect("Scaffold operation log")
                .is_empty()
        );
    }

    #[test]
    fn reconnect_supervisor_gates_flips_and_dedupes() {
        let config = EngineBootConfig {
            data_dir: PathBuf::from("/tmp/comet-reconnect-test"),
            ipc_port: 0,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            project_scope: "test".into(),
            deployment_id: None,
            scaffold_url: None,
            default_harness: HarnessId::Mock,
            runtime_profile: RuntimeProfile::Mock,
        };
        let mut state = AppState::new();

        // Closure before the first attach (still Connecting) never reconnects.
        state.boot_config = Some(config);
        assert!(state.reconnect_config().is_none());

        // An attached app reconnects: the first detector wins and flips the
        // status back to Connecting…
        state.connection = ConnectionStatus::Ready;
        assert!(state.reconnect_config().is_some());
        assert!(
            matches!(state.connection, ConnectionStatus::Connecting),
            "a closed transport must flip the app back to Connecting"
        );
        // …so the other watch tasks noticing the same dead transport collapse
        // into that one bootstrap instead of racing their own.
        assert!(state.reconnect_config().is_none());

        // Quit guard: after begin_shutdown the drained engine's closing
        // streams must not restart a fresh engine mid-teardown.
        state.connection = ConnectionStatus::Ready;
        assert!(state.begin_shutdown().is_none()); // no engine attached here
        assert!(state.reconnect_config().is_none());
        assert!(matches!(state.connection, ConnectionStatus::Ready));

        // A state that never bootstrapped has no config to reboot with.
        let mut blank = AppState::new();
        blank.connection = ConnectionStatus::Ready;
        assert!(blank.reconnect_config().is_none());
    }

    #[test]
    fn scaffold_starting_state_survives_materialization_and_clears_on_delete() {
        let mut state = AppState::new();
        state.scaffold_scope = Some(("ashler-staging".into(), "ashler-staging".into()));
        state.mark_chat_pending("scaffold-chat");
        state.mark_scaffold_chat_starting("scaffold-chat");

        state.apply_chats(Vec::new());
        assert!(state.scaffold_chat_starting("scaffold-chat"));

        state.apply_chats(vec![chat("scaffold-chat", 0, None)]);
        assert!(state.scaffold_chat_starting("scaffold-chat"));

        state.apply_chats(Vec::new());
        assert!(!state.scaffold_chat_starting("scaffold-chat"));
    }

    fn local_candidate(
        id: &str,
        chat_id: &str,
        resumable: bool,
        history_only: bool,
    ) -> LocalSessionCandidate {
        LocalSessionCandidate {
            id: id.into(),
            chat_id: chat_id.into(),
            harness: if resumable {
                HarnessId::PrimeAgent
            } else {
                HarnessId::Codex
            },
            session_id: format!("native-{id}"),
            cwd: "/workspace/comet".into(),
            title: format!("Session {id}"),
            preview: None,
            model: None,
            reasoning: None,
            created_at: 1,
            updated_at: 2,
            live_attachable: false,
            resumable,
            history_only,
            busy_elsewhere: None,
        }
    }

    fn space(id: &str, device_id: &str, path: &str, created_min: i64) -> Space {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Space {
            id: id.into(),
            device_id: device_id.into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: base + TimeDelta::minutes(created_min),
        }
    }

    fn session(
        chat_id: &str,
        status: SessionStatus,
        updated_secs_ago: i64,
        now: DateTime<Utc>,
    ) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: "dev".into(),
            status,
            started_at: None,
            updated_at: now - TimeDelta::seconds(updated_secs_ago),
        }
    }

    #[test]
    fn chats_sort_by_last_message_desc_with_created_fallback() {
        let mut chats = vec![
            chat("a", 0, Some(10)),
            chat("b", 5, None), // no messages → keys on created_at (+5min)
            chat("c", 1, Some(30)),
            chat("d", 40, None), // created after every message
        ];
        sort_chats(&mut chats);
        let order: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["d", "c", "a", "b"]);
    }

    #[test]
    fn chat_sort_ties_are_deterministic() {
        let mut chats = vec![chat("z", 0, Some(10)), chat("a", 0, Some(10))];
        sort_chats(&mut chats);
        assert_eq!(chats[0].id, "a");
    }

    #[test]
    fn working_indicator_staleness() {
        let now = Utc::now();
        // Fresh working session shows.
        let fresh = session("c", SessionStatus::Working, 10, now);
        assert_eq!(effective_indicator(Some(&fresh), now), Indicator::Working);
        // Stale working session is suppressed — crashed backend, not eternal spinner.
        let stale = session("c", SessionStatus::Working, 46, now);
        assert_eq!(effective_indicator(Some(&stale), now), Indicator::None);
        // Exactly at the boundary still shows (strictly-older-than semantics).
        let edge = session("c", SessionStatus::Working, 45, now);
        assert_eq!(effective_indicator(Some(&edge), now), Indicator::Working);
        // Future timestamps (clock skew) count as fresh.
        let skewed = session("c", SessionStatus::Working, -30, now);
        assert_eq!(effective_indicator(Some(&skewed), now), Indicator::Working);
    }

    #[test]
    fn indicator_kinds() {
        let now = Utc::now();
        assert_eq!(effective_indicator(None, now), Indicator::None);
        let idle = session("c", SessionStatus::Idle, 0, now);
        assert_eq!(effective_indicator(Some(&idle), now), Indicator::None);
        // Errored is not staleness-gated: the error stays visible.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(effective_indicator(Some(&errored), now), Indicator::Errored);
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            effective_indicator(Some(&awaiting), now),
            Indicator::AwaitingInput
        );
        let awaiting_stale = session("c", SessionStatus::AwaitingInput, 300, now);
        assert_eq!(
            effective_indicator(Some(&awaiting_stale), now),
            Indicator::None
        );
    }

    #[test]
    fn display_status_derivation() {
        let now = Utc::now();
        let mut c = chat("c", 0, Some(10));
        // Live states win regardless of seen.
        let working = session("c", SessionStatus::Working, 5, now);
        assert_eq!(
            display_status(&c, Some(&working), now),
            ChatIndicator::Working
        );
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            display_status(&c, Some(&awaiting), now),
            ChatIndicator::AwaitingInput
        );
        // Finished + unseen = Completed (no session row at all).
        assert_eq!(display_status(&c, None, now), ChatIndicator::Completed);
        // Idle session + unseen = Completed.
        let idle = session("c", SessionStatus::Idle, 5, now);
        assert_eq!(
            display_status(&c, Some(&idle), now),
            ChatIndicator::Completed
        );
        // Stale working session falls back to the seen check.
        let stale = session("c", SessionStatus::Working, 300, now);
        assert_eq!(
            display_status(&c, Some(&stale), now),
            ChatIndicator::Completed
        );
        // Seen after the last message = Idle.
        c.last_seen_at = c.last_message_at.map(|t| t + TimeDelta::minutes(1));
        assert_eq!(display_status(&c, Some(&idle), now), ChatIndicator::Idle);
        // Errored + unseen = Errored; seen clears it to Idle.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(display_status(&c, Some(&errored), now), ChatIndicator::Idle);
        c.last_seen_at = None;
        assert_eq!(
            display_status(&c, Some(&errored), now),
            ChatIndicator::Errored
        );
        // No messages at all: nothing to see — Idle.
        let fresh = chat("f", 0, None);
        assert_eq!(display_status(&fresh, None, now), ChatIndicator::Idle);
    }

    #[test]
    fn active_list_sorts_by_recency_only_status_never_moves_rows() {
        let a = chat("a", 0, Some(10)); // Completed (older)
        let b = chat("b", 0, Some(20)); // Completed (newer)
        let c = chat("c", 0, Some(5)); // AwaitingInput
        let d = chat("d", 0, Some(1)); // Working
        let mut rows = vec![
            (ChatIndicator::Completed, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut rows);
        let order: Vec<&str> = rows.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a", "c", "d"], "recency desc, status ignored");

        // Opening a completed session (completed → seen → idle) must NOT
        // change its position (user report: rows jumped under the pointer).
        let mut seen = vec![
            (ChatIndicator::Idle, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut seen);
        let order_after: Vec<&str> = seen.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, order_after);
    }

    #[test]
    fn tabs_order_by_creation_not_activity() {
        let a = chat("a", 5, Some(100)); // created later, very active
        let b = chat("b", 1, Some(2));
        let mut tabs = vec![&a, &b];
        sort_tabs(&mut tabs);
        let order: Vec<&str> = tabs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a"]);
    }

    #[test]
    fn apply_spaces_sorts_and_heals_selection() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s2", "dev", "/b", 2),
            space("s1", "dev", "/a", 1),
            space("comet-scaffold-space-dev", "dev", "/legacy-scaffold", 3),
        ]);
        let ids: Vec<&str> = state.spaces.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2"]);
        // First frame auto-selects the first space.
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        state.selected_space = Some("s2".into());
        // Vanished selection heals to the first space.
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        // No spaces at all: selection clears.
        state.apply_spaces(vec![]);
        assert_eq!(state.selected_space, None);
    }

    #[test]
    fn chats_in_space_filters_and_orders() {
        let mut state = AppState::new();
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        let mut in_space_new = chat("new", 5, None);
        in_space_new.space_id = Some("s1".into());
        let mut in_space_old = chat("old", 1, Some(50)); // active but created first
        in_space_old.space_id = Some("s1".into());
        let mut other = chat("other", 2, None);
        other.space_id = Some("s2".into());
        let mut archived = chat("gone", 0, None);
        archived.space_id = Some("s1".into());
        archived.archived = true;
        let dangling = chat("dangling", 3, None); // no space id
        let mut legacy_scaffold = chat("legacy-scaffold", 6, Some(60));
        legacy_scaffold.space_id = Some("comet-scaffold-space-dev".into());
        state.apply_chats(vec![
            in_space_new,
            in_space_old,
            other,
            archived,
            dangling,
            legacy_scaffold,
        ]);
        let ids: Vec<&str> = state
            .chats_in_space("s1")
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["old", "new"]);
        assert!(!state.chats.iter().any(|chat| chat.id == "legacy-scaffold"));
        // The overview shows every live-space chat (idle included) — chats of
        // unknown spaces stay hidden. Completed ("old") outranks idle ("new").
        let now = Utc::now();
        let overview: Vec<&str> = state
            .overview_chats(now)
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(overview, ["old", "new"]);
    }

    #[test]
    fn selected_logical_source_includes_chats_from_every_member_space() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("main", "dev", "/repo", 1),
            space("worktree", "dev", "/worktree", 2),
        ]);
        let mut main_chat = chat("main-chat", 1, None);
        main_chat.space_id = Some("main".into());
        let mut worktree_chat = chat("worktree-chat", 2, None);
        worktree_chat.space_id = Some("worktree".into());
        state.apply_chats(vec![main_chat, worktree_chat]);
        state.selected_space = Some("main".into());
        state.selected_space_members = vec!["main".into(), "worktree".into()];

        let ids: Vec<&str> = state
            .chats_in_selected_source()
            .iter()
            .map(|chat| chat.id.as_str())
            .collect();

        assert_eq!(ids, ["main-chat", "worktree-chat"]);
    }

    #[test]
    fn apply_chats_drops_vanished_selection() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        state.selected_chat = Some("a".into());
        state.transcript = vec![];
        state.apply_chats(vec![chat("b", 1, None)]);
        assert_eq!(state.selected_chat, None);
        // Still-present selection survives.
        state.selected_chat = Some("b".into());
        state.apply_chats(vec![chat("b", 1, None), chat("c", 2, None)]);
        assert_eq!(state.selected_chat.as_deref(), Some("b"));
    }

    #[test]
    fn local_candidates_disappear_once_their_comet_chat_exists() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("local-chat-existing", 0, None)]);
        state.apply_local_session_candidates(vec![
            local_candidate("existing", "local-chat-existing", false, true),
            local_candidate("fresh", "local-chat-fresh", true, false),
        ]);
        assert_eq!(
            state
                .local_session_candidates
                .iter()
                .map(|candidate| candidate.id.as_str())
                .collect::<Vec<_>>(),
            ["fresh"]
        );

        state.apply_chats(vec![
            chat("local-chat-existing", 0, None),
            chat("local-chat-fresh", 1, None),
        ]);
        assert!(state.local_session_candidates.is_empty());
    }

    #[test]
    fn pending_chat_selection_survives_until_the_chats_watch_catches_up() {
        let mut state = AppState::new();
        state.selected_chat = Some("local-chat-fresh".into());
        state.mark_chat_pending("local-chat-fresh");

        state.apply_chats(Vec::new());
        assert_eq!(state.selected_chat.as_deref(), Some("local-chat-fresh"));

        state.apply_chats(vec![chat("local-chat-fresh", 0, None)]);
        assert_eq!(state.selected_chat.as_deref(), Some("local-chat-fresh"));
        assert!(state.pending_local_chat_ids.is_empty());
    }

    #[test]
    fn apply_chat_config_stamps_the_row() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        let config = comet_proto::ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("claude-fable-5".into()),
            reasoning: Some(comet_proto::ReasoningLevel::XHigh),
            agent_account_id: Some("opaque-account-id".into()),
            model_options: serde_json::Map::new(),
            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
        };
        state.apply_chat_config("a", config.clone());
        assert_eq!(
            state.chats.iter().find(|c| c.id == "a").unwrap().config,
            Some(config)
        );
        assert!(
            state
                .chats
                .iter()
                .find(|c| c.id == "b")
                .unwrap()
                .config
                .is_none()
        );
        // Unknown chat: no-op, no panic.
        state.apply_chat_config(
            "missing",
            comet_proto::ChatConfig {
                harness: HarnessId::ClaudeCode,
                model: None,
                reasoning: None,
                agent_account_id: None,
                model_options: serde_json::Map::new(),
                sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
            },
        );
    }

    #[test]
    fn visible_chats_filters_archived() {
        let mut state = AppState::new();
        let mut archived = chat("a", 0, Some(99));
        archived.archived = true;
        state.apply_chats(vec![archived, chat("b", 1, None)]);
        let visible: Vec<&str> = state.visible_chats().map(|c| c.id.as_str()).collect();
        assert_eq!(visible, ["b"]);
    }

    #[test]
    fn settled_chats_are_archived_live_space_rows_in_recency_order() {
        let mut state = AppState::new();
        state.apply_spaces(vec![space("space", "dev", "/workspace", 0)]);
        let mut older = chat("older", 0, Some(2));
        older.archived = true;
        older.space_id = Some("space".into());
        let mut newer = chat("newer", 0, Some(5));
        newer.archived = true;
        newer.space_id = Some("space".into());
        let mut dangling = chat("dangling", 0, Some(9));
        dangling.archived = true;
        dangling.space_id = Some("missing".into());
        let mut active = chat("active", 0, Some(10));
        active.space_id = Some("space".into());
        state.apply_chats(vec![older, newer, dangling, active]);

        let settled: Vec<&str> = state
            .settled_chats()
            .into_iter()
            .map(|chat| chat.id.as_str())
            .collect();
        assert_eq!(settled, ["newer", "older"]);
    }

    #[test]
    fn echoes_show_until_doc_frame_confirms() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        let echo = SessionMessageEntry {
            id: "m1".into(),
            role: comet_doc::MessageRole::User,
            parts: vec![],
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        state.push_echo("c1", echo.clone());
        // Duplicate pushes dedupe.
        state.push_echo("c1", echo.clone());
        assert_eq!(state.pending_echoes().len(), 1);
        // Frames without the id keep the echo.
        state.apply_transcript(vec![]);
        assert_eq!(state.pending_echoes().len(), 1);
        // The confirming frame prunes it.
        state.apply_transcript(vec![SessionMessageEntry {
            id: "m1".into(),
            ..echo.clone()
        }]);
        assert!(state.pending_echoes().is_empty());
        // Failure path: explicit removal.
        state.push_echo(
            "c1",
            SessionMessageEntry {
                id: "m2".into(),
                ..echo.clone()
            },
        );
        state.remove_echo("c1", "m2");
        assert!(state.pending_echoes().is_empty());
        // Echoes are per chat.
        state.push_echo(
            "other",
            SessionMessageEntry {
                id: "m3".into(),
                ..echo
            },
        );
        assert!(state.pending_echoes().is_empty());
    }

    #[test]
    fn gate_phases() {
        let user = UserProfile {
            id: "u".into(),
            email: "w@example.com".into(),
            name: None,
        };
        assert_eq!(
            gate_phase(&ConnectionStatus::Connecting, None),
            GatePhase::Loading
        );
        assert_eq!(
            gate_phase(&ConnectionStatus::Failed("boom".into()), None),
            GatePhase::Failed("boom".into())
        );
        // Unknown auth (pre-M4) gates nothing.
        assert_eq!(gate_phase(&ConnectionStatus::Ready, None), GatePhase::Ready);
        assert_eq!(
            gate_phase(&ConnectionStatus::Ready, Some(&AuthState::SignedOut)),
            GatePhase::SignIn
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(&AuthState::SignedIn {
                    user,
                    project_scope: "project-test".into(),
                })
            ),
            GatePhase::Ready
        );
    }

    #[test]
    fn auth_frames_parse_current_wire_shape() {
        let proto = serde_json::json!({ "state": "signedOut" });
        assert_eq!(parse_auth_state(&proto), Some(AuthState::SignedOut));
        let engine = serde_json::json!({
            "_tag": "SignedIn",
            "user": { "id": "u1", "email": "w@example.com" },
            "projectScope": "project-test",
        });
        let Some(AuthState::SignedIn {
            user,
            project_scope,
        }) = parse_auth_state(&engine)
        else {
            panic!("expected SignedIn");
        };
        assert_eq!(user.email, "w@example.com");
        assert_eq!(project_scope, "project-test");
        // Garbage is dropped rather than crashing the auth stream.
        assert_eq!(
            parse_auth_state(&serde_json::json!({ "_tag": "Wat" })),
            None
        );
        assert_eq!(parse_auth_state(&serde_json::json!(42)), None);
    }

    fn chat_with_cwd(id: &str, created_min: i64, cwd: Option<&str>) -> Chat {
        let mut c = chat(id, created_min, None);
        c.cwd = cwd.map(str::to_string);
        c
    }

    #[test]
    fn project_labels_from_cwd() {
        assert_eq!(project_label(Some("/home/w/dev/comet")), "comet");
        assert_eq!(project_label(Some("/home/w/dev/comet/")), "comet");
        assert_eq!(project_label(None), "No project");
        assert_eq!(project_label(Some("   ")), "No project");
        assert_eq!(project_label(Some("/")), "/");
    }

    #[test]
    fn grouped_sidebar_preserves_recency_order() {
        // Input is sidebar-sorted (most recent first).
        let chats = [
            chat_with_cwd("a", 9, Some("/dev/comet")),
            chat_with_cwd("b", 8, Some("/dev/zed")),
            chat_with_cwd("c", 7, Some("/dev/comet")),
            chat_with_cwd("d", 6, None),
        ];
        let groups = group_chats(chats.iter());
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        // Groups ordered by their most recent chat; rows keep order.
        assert_eq!(labels, ["comet", "zed", "No project"]);
        let comet_ids: Vec<&str> = groups[0].chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(comet_ids, ["a", "c"]);
        assert!(group_chats(std::iter::empty()).is_empty());
    }

    #[test]
    fn relative_times_match_comet_format() {
        let now = Utc::now();
        let ago = |secs: i64| now - chrono::Duration::seconds(secs);
        assert_eq!(format_time_ago(ago(0), now), "now");
        assert_eq!(format_time_ago(ago(59), now), "now");
        assert_eq!(format_time_ago(ago(60), now), "1m");
        assert_eq!(format_time_ago(ago(59 * 60), now), "59m");
        assert_eq!(format_time_ago(ago(60 * 60), now), "1h");
        assert_eq!(format_time_ago(ago(23 * 3600 + 3599), now), "23h");
        assert_eq!(format_time_ago(ago(24 * 3600), now), "1d");
        assert_eq!(format_time_ago(ago(6 * 86400), now), "6d");
        assert_eq!(format_time_ago(ago(7 * 86400), now), "1w");
        assert_eq!(format_time_ago(ago(30 * 86400), now), "4w");
        assert_eq!(format_time_ago(ago(35 * 86400), now), "1mo");
        assert_eq!(format_time_ago(ago(400 * 86400), now), "1y");
        // Clock skew (future timestamps) clamps to "now".
        assert_eq!(
            format_time_ago(now + chrono::Duration::hours(2), now),
            "now"
        );
    }

    #[test]
    fn chat_location_joins_project_and_branch() {
        let mut c = chat_with_cwd("x", 1, Some("/home/w/dev/soccertcg"));
        c.branch = Some("comet/rebalance".into());
        assert_eq!(
            chat_location(&c).as_deref(),
            Some("soccertcg · comet/rebalance")
        );
        c.branch = None;
        assert_eq!(chat_location(&c).as_deref(), Some("soccertcg"));
        c.cwd = None;
        c.branch = Some("main".into());
        assert_eq!(chat_location(&c).as_deref(), Some("main"));
        c.branch = Some("   ".into());
        assert_eq!(chat_location(&c), None);
        c.branch = None;
        assert_eq!(chat_location(&c), None);
    }

    #[test]
    fn invitation_routes_to_exact_session_and_verified_grant() {
        let now = Utc::now().timestamp_millis();
        let snapshot: CollaborationSnapshot = serde_json::from_value(serde_json::json!({
            "schemaVersion": 2,
            "sessions": [
                {
                    "sessionId": "session-other",
                    "chatId": "chat-a",
                    "ownerSubject": "iap:other@example.com",
                    "ownerDeviceId": "device-other",
                    "source": "local",
                    "createdAt": now
                },
                {
                    "sessionId": "session-invited",
                    "chatId": "chat-a",
                    "ownerSubject": "iap:owner@example.com",
                    "ownerDeviceId": "device-owner",
                    "source": "local",
                    "createdAt": now
                }
            ],
            "principal": {
                "subject": "iap:invitee@example.com",
                "projectId": "project-a",
                "deploymentId": "deployment-a",
                "sessionId": "session-invited",
                "capabilities": ["session.read"]
            },
            "grants": [{
                "id": "grant-invited",
                "principalSubject": "iap:invitee@example.com",
                "scope": {
                    "projectId": "project-a",
                    "deploymentId": "deployment-a",
                    "sessionId": "session-invited"
                },
                "capabilities": ["session.read"],
                "deviceId": "device-owner",
                "grantedBy": "iap:owner@example.com",
                "grantedAt": now - 1,
                "expiresAt": now + 60_000
            }]
        }))
        .unwrap();
        let mut state = AppState::new();
        state.selected_chat = Some("chat-a".into());
        state.pending_invitation =
            comet_proto::CometInvitation::new("chat-a", "session-invited", "grant-invited");

        state.apply_collaboration(snapshot);

        assert_eq!(
            state.selected_agent_session.as_deref(),
            Some("session-invited")
        );
        assert_eq!(
            state.selected_invitation_grant.as_deref(),
            Some("grant-invited")
        );
        assert!(state.pending_invitation.is_none());
        // No chat row for the invited session: verified membership arms the
        // sidebar pin that replaces the manual exact-id import dialog.
        assert_eq!(state.pending_session_pin.as_deref(), Some("chat-a"));
    }
    #[test]
    fn imported_membership_keeps_selection_without_a_chat_row() {
        let chat_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let mut state = AppState::new();
        state.selected_chat = Some(chat_id.into());
        state.apply_session_refs(vec![SessionRef {
            chat_id: chat_id.into(),
            added_at: Utc::now(),
        }]);

        state.apply_chats(Vec::new());
        assert_eq!(state.selected_chat.as_deref(), Some(chat_id));
        assert!(state.selected_chat_row().is_none());
        assert!(state.is_shared_session(chat_id));

        state.apply_session_refs(Vec::new());
        assert!(state.selected_chat.is_none());
    }

    #[test]
    fn imported_session_title_promotes_from_id_to_transcript_preview() {
        let chat_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let mut state = AppState::new();
        state.selected_chat = Some(chat_id.into());
        state.apply_session_refs(vec![SessionRef {
            chat_id: chat_id.into(),
            added_at: Utc::now(),
        }]);
        assert_eq!(state.shared_session_title(chat_id), "Session aaaaaaaa");

        state.apply_transcript(vec![SessionMessageEntry {
            id: "message".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "text".into(),
                text: "  Explain   the shared transcript  ".into(),
            }],
            created_at: 1,
            device_id: "remote".into(),
            status: None,
            continuation_of: None,
        }]);
        assert_eq!(
            state.shared_session_title(chat_id),
            "Explain the shared transcript"
        );
    }

    #[test]
    fn chat_row_wins_over_imported_membership() {
        let chat_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let mut state = AppState::new();
        state.apply_session_refs(vec![SessionRef {
            chat_id: chat_id.into(),
            added_at: Utc::now(),
        }]);
        state.apply_chats(vec![chat(chat_id, 1, None)]);

        // The row carries space/status/harness context — it renders in the
        // normal sidebar; the membership pin stays out of the Shared list.
        assert_eq!(state.shared_session_refs().count(), 0);
        assert_eq!(state.visible_chats().count(), 1);
        assert!(!state.is_shared_session(chat_id));

        state.apply_chats(Vec::new());
        assert_eq!(state.shared_session_refs().count(), 1);
        assert!(state.is_shared_session(chat_id));
    }
}
