//! comet-engine — the headless backend: sessions engine, doc host + command executor,
//! run journal + crash recovery, and the IPC RPC server.
//!
//! Spec: ARCHITECTURE.md §5 and docs/research/feature-inventory.md §3. M2 surface:
//! sessions + docs + commands + minimal IPC. Terminals, repos/diffs, uploads, auth,
//! agent accounts, and the device-room host land in later milestones.

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use comet_proto::{HarnessId, RuntimeProfile};

use comet_sync::DocsStore;

pub mod agent_accounts;
pub mod auth;
pub mod diff_sync;
pub mod doc_host;
mod inference_relay;
pub mod instance_lock;
pub mod local_sessions;
pub mod omp_session_artifact;
pub mod registry;
pub mod repos;
pub mod rpc;
pub mod run_journal;
pub mod scaffold;
pub mod sessions;
pub mod spaces;
pub mod terminals;
pub mod titles;
pub mod uploads;
pub mod workspace_host;

pub use agent_accounts::{AgentAccounts, AgentAccountsConfig};
pub use auth::{Auth, AuthConfig, AuthState, AuthUser};
pub use diff_sync::{CheckoutDiffSync, DiffSidecar, DiffSnapshot, capture_diff};
pub use doc_host::{ChatDocHandle, DocHost, DocHostConfig, EdgeConfig};
pub use instance_lock::InstanceLock;
pub use local_sessions::capture_omp_artifact;
pub use omp_session_artifact::MAX_OMP_SESSION_ARTIFACT_BYTES;
pub use registry::{HarnessDescriptor, HarnessRegistry, default_registry};
pub use repos::{CheckoutIdentity, Repos, worktree_branch_from_title};
pub use rpc::EngineRpc;
pub use run_journal::{JournalError, RunJournal};
pub use scaffold::{
    DeviceJoinGrantProvider, EdgeDeviceJoinGrantClient, ScaffoldClient, ScaffoldError,
    ScaffoldRuntime, UnavailableDeviceJoinGrantProvider,
};
pub use sessions::{JournaledEvent, SessionsEngine, SteerOutcome};
pub use spaces::SpacesSync;
pub use terminals::Terminals;
pub use titles::TitleGenerator;
pub use uploads::{AttachmentChunk, Uploads};
pub use workspace_host::{
    DEFAULT_PROJECT_SCOPE, DEFAULT_USER_ID, WORKSPACE_DOC_ID, WorkspaceHost, WorkspaceHostConfig,
};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("doc: {0}")]
    Doc(#[from] comet_doc::DocError),
    #[error("journal: {0}")]
    Journal(#[from] run_journal::JournalError),
    #[error("store: {0}")]
    Store(#[from] comet_sync::StoreError),
    #[error("harness: {0}")]
    Harness(#[from] comet_harness::HarnessError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Epoch millis now — the doc/journal timestamp base.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Data directory (default `~/.comet-native`, dev `~/.comet-native-dev`).
    pub data_dir: PathBuf,
    /// Edge base URL.
    pub edge_url: String,
    /// Bearer for edge room joins; `None` runs fully offline (sync disabled).
    pub edge_token: Option<String>,
    /// Localhost IPC port for the UI.
    pub ipc_port: u16,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// Server-enforced capabilities for this engine process.
    pub runtime_profile: RuntimeProfile,
    /// Operator-configured Scaffold deployment/project scope.
    pub project_scope: String,
    /// Trusted deployment namespace for a Scaffold-host SessionRoom.
    pub deployment_id: Option<String>,
    /// Scaffold control-plane origin; `None` enables explicit dev bearer mode.
    pub scaffold_url: Option<String>,
}

#[derive(Clone)]
pub struct DeviceBootstrapConfig {
    pub device_join_grant: String,
    pub project_id: String,
    pub deployment_id: String,
    pub session_id: String,
    pub device_id: String,
    pub lifecycle_epoch: u64,
    pub sandbox_id: String,
}

impl std::fmt::Debug for DeviceBootstrapConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceBootstrapConfig")
            .field("device_join_grant", &"<redacted>")
            .field("project_id", &self.project_id)
            .field("deployment_id", &self.deployment_id)
            .field("session_id", &self.session_id)
            .field("device_id", &self.device_id)
            .field("lifecycle_epoch", &self.lifecycle_epoch)
            .field("sandbox_id", &self.sandbox_id)
            .finish()
    }
}

struct AssemblyContext<'a> {
    project_scope: &'a str,
    user_id: &'a str,
    runtime_profile: RuntimeProfile,
    ipc_port: u16,
}

/// The assembled engine core — also constructible without the IPC server for tests
/// and the in-process (headed) mode.
pub struct EngineCore {
    pub sessions: SessionsEngine,
    pub doc_host: DocHost,
    pub workspace: WorkspaceHost,
    pub registry: Arc<HarnessRegistry>,
    pub repos: Repos,
    pub terminals: Terminals,
    pub diff_sync: CheckoutDiffSync,
    pub spaces_sync: SpacesSync,
    pub uploads: Uploads,
    pub agent_accounts: AgentAccounts,
    pub device_id: String,
    pub runtime_profile: RuntimeProfile,
    /// Auth service (attached by [`Engine::run`]; a lazy dev-mode instance otherwise).
    auth: std::sync::Mutex<Option<Auth>>,
    /// Peer link cache for `targetDeviceId` routing (attached when edge+auth are ready).
    links: std::sync::Mutex<Option<Arc<comet_rpc::LinkCache>>>,
    /// Release checker (attached by [`Engine::assemble_runtime`]) — the
    /// UpdateStatus stream + ApplyUpdate.
    updater: std::sync::Mutex<Option<comet_update::Updater>>,
    /// Optional Scaffold control-plane integration, exposed over the native RPC service.
    scaffold: std::sync::Mutex<Option<ScaffoldRuntime>>,
    /// Exclusive data-dir lock — held for the engine's lifetime (single-instance).
    _instance_lock: InstanceLock,
}

impl EngineCore {
    /// Open stores under `data_dir`, wire sessions ⇄ doc host ⇄ workspace host, and
    /// recover stale journals from a previous crash. Identity comes from
    /// `$COMET_PROJECT_SCOPE` / `$COMET_USER_ID`; use
    /// [`Self::assemble_with_identity`] to pass one explicitly.
    pub fn assemble(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
    ) -> Result<Self, EngineError> {
        let project_scope = env_or("COMET_PROJECT_SCOPE", "ashler-local");
        let user_id = env_or("COMET_USER_ID", DEFAULT_USER_ID);
        Self::assemble_with_identity_and_ipc_port(
            data_dir,
            registry,
            default_harness,
            edge,
            AssemblyContext {
                project_scope: &project_scope,
                user_id: &user_id,
                runtime_profile: if default_harness == HarnessId::Mock {
                    RuntimeProfile::Mock
                } else {
                    RuntimeProfile::LocalController
                },
                ipc_port: ipc_port_from_env(),
            },
        )
    }

    pub fn assemble_with_identity(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        project_scope: &str,
        user_id: &str,
        runtime_profile: RuntimeProfile,
    ) -> Result<Self, EngineError> {
        Self::assemble_with_identity_and_ipc_port(
            data_dir,
            registry,
            default_harness,
            edge,
            AssemblyContext {
                project_scope,
                user_id,
                runtime_profile,
                ipc_port: ipc_port_from_env(),
            },
        )
    }

    fn assemble_with_identity_and_ipc_port(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        context: AssemblyContext<'_>,
    ) -> Result<Self, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        // Single-instance guard: two engines on one data dir would race the
        // SQLite snapshots + journals. Taken before any store opens or the IPC
        // port binds; held (and kernel-released on crash) for the engine's life.
        let lock = InstanceLock::acquire(data_dir)?;
        let device_id = load_or_create_device_id(data_dir)?;
        // Identity-scoped storage. `projects/` is a clean boundary from the
        // removed application-organization layout.
        let project_dir = data_dir
            .join("projects")
            .join(sanitize_path_id(context.project_scope))
            .join(sanitize_path_id(context.user_id));
        let store = Arc::new(DocsStore::open(&project_dir)?);
        let journal = Arc::new(RunJournal::open(project_dir.join("journals"))?);
        let sessions = SessionsEngine::new(
            device_id.clone(),
            journal,
            registry.clone(),
            context.ipc_port,
        );
        let doc_host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: device_id.clone(),
                default_harness,
                edge: edge.clone(),
            },
        );
        let workspace = WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: device_id.clone(),
                device_name: local_device_name(),
                platform: std::env::consts::OS.to_string(),
                project_scope: context.project_scope.to_string(),
                user_id: context.user_id.to_string(),
                edge: context
                    .runtime_profile
                    .allows_workspace_room()
                    .then(|| edge.clone())
                    .flatten(),
            },
        )?;
        doc_host.set_workspace(workspace.clone());
        doc_host.set_sessions(sessions.clone());
        sessions.set_doc_host(doc_host.clone());
        match sessions.recover_stale() {
            Ok(0) => {}
            Ok(recovered) => tracing::info!(recovered, "stale sessions recovered on boot"),
            Err(err) => tracing::error!(error = %err, "stale-session recovery failed"),
        }
        let repos = Repos::new(data_dir, &device_id);
        let terminals = Terminals::new();
        let uploads = Uploads::new(data_dir, edge.clone());
        let agent_accounts = AgentAccounts::new(AgentAccountsConfig::detect(data_dir));
        sessions.set_titles(TitleGenerator::new(workspace.clone(), repos.clone()));
        let diff_sync = CheckoutDiffSync::start(repos.clone(), workspace.clone(), &device_id, edge);
        let spaces_sync = SpacesSync::start(repos.clone(), workspace.clone(), &device_id);
        Ok(Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            diff_sync,
            spaces_sync,
            uploads,
            agent_accounts,
            device_id,
            runtime_profile: context.runtime_profile,
            auth: std::sync::Mutex::new(None),
            links: std::sync::Mutex::new(None),
            updater: std::sync::Mutex::new(None),
            scaffold: std::sync::Mutex::new(None),
            _instance_lock: lock,
        })
    }

    /// Attach the auth service (before building the RPC service / relays).
    pub fn set_auth(&self, auth: Auth) {
        *self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(auth);
    }

    /// The attached auth service, or a lazily-created explicit dev-mode one.
    pub fn auth(&self) -> Auth {
        let mut slot = self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.get_or_insert_with(|| {
            let dev_user = std::env::var("COMET_EDGE_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "dev-user".into());
            let mut config = AuthConfig::new("http://localhost:27640", std::env::temp_dir());
            config.project_scope = self.workspace.project_scope().to_string();
            config.dev_user_id = dev_user;
            Auth::new(config)
        })
        .clone()
    }

    /// Attach the peer link cache — enables `targetDeviceId` routing and [`Self::dial_device`].
    pub fn set_links(&self, links: Arc<comet_rpc::LinkCache>) {
        *self
            .links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(links);
    }

    pub fn links(&self) -> Option<Arc<comet_rpc::LinkCache>> {
        self.links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Attach the release checker (before building the RPC service).
    pub fn set_updater(&self, updater: comet_update::Updater) {
        *self
            .updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(updater);
    }

    pub fn updater(&self) -> Option<comet_update::Updater> {
        self.updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_scaffold_runtime(&self, scaffold: ScaffoldRuntime) {
        self.agent_accounts.set_remote(scaffold.client());
        *self
            .scaffold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(scaffold);
    }

    pub fn scaffold_runtime(&self) -> Option<ScaffoldRuntime> {
        self.scaffold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A live RPC client to another device's engine through its relay DO (the router's
    /// dial seam). Cached per device; invalidated + re-dialed on failure.
    pub async fn dial_device(
        &self,
        device_id: &str,
    ) -> Result<Arc<comet_rpc::RpcClient>, EngineError> {
        let links = self
            .links()
            .ok_or_else(|| EngineError::Other("peer links unavailable (offline)".into()))?;
        links
            .client(device_id)
            .await
            .map_err(|e| EngineError::Other(e.to_string()))
    }

    /// Start hosting our device room: serve the full RPC surface to relay clients and
    /// warm-open chat docs on nudges (§7 cold-chat command delivery). The token source
    /// re-reads auth on every (re)dial, so token refreshes take effect at reconnect.
    pub fn start_host_relay(&self, edge_url: &str) -> comet_rpc::HostRelay {
        let auth = self.auth();
        let config =
            comet_rpc::HostRelayConfig::new(edge_url, self.device_id.clone(), Arc::new(auth));
        let nudge_host = self.doc_host.clone();
        let on_nudge: comet_rpc::NudgeHandler = Arc::new(move |chat_id: String| {
            // Scaffold hosts must not open the legacy unprojected room while
            // the verified grant frame is still in flight.
            match nudge_host.open_for_nudge(&chat_id) {
                Ok(Some(_)) => tracing::info!(chat = %chat_id, "nudge: chat doc opened"),
                Ok(None) => {
                    tracing::info!(chat = %chat_id, "nudge: waiting for verified room grant")
                }
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "nudge: open failed")
                }
            }
        });
        let reset_host = self.doc_host.clone();
        let on_grant_reset: comet_rpc::GrantResetHandler =
            Arc::new(move || reset_host.reset_edge_grants());
        let grant_host = self.doc_host.clone();
        let on_grant: comet_rpc::GrantHandler = Arc::new(move |session_id, payload| {
            if let Err(reason) = grant_host.ingest_verified_grant(&session_id, &payload) {
                tracing::warn!(reason, "device-room: rejected authority grant frame");
            }
        });
        comet_rpc::HostRelay::spawn_with_authority(
            config,
            self.rpc_service(),
            on_nudge,
            on_grant_reset,
            on_grant,
        )
    }

    pub fn rpc_service(&self) -> Arc<EngineRpc> {
        let mut rpc = EngineRpc::new(
            self.sessions.clone(),
            self.doc_host.clone(),
            self.workspace.clone(),
            self.registry.clone(),
            self.repos.clone(),
            self.terminals.clone(),
            self.diff_sync.clone(),
            self.uploads.clone(),
            self.agent_accounts.clone(),
            self.runtime_profile,
        )
        .with_auth(self.auth());
        if let Some(links) = self.links() {
            rpc = rpc.with_links(links);
        }
        if let Some(updater) = self.updater() {
            rpc = rpc.with_updater(updater);
        }
        if let Some(scaffold) = self.scaffold_runtime() {
            rpc = rpc.with_scaffold(scaffold);
        }
        Arc::new(rpc)
    }

    /// Graceful teardown: settle live runs (streaming entries stamped `aborted`),
    /// kill live PTYs, stamp our workspace `lastSeenAt`, and flush every open doc
    /// snapshot.
    pub async fn shutdown(&self) {
        self.sessions.shutdown().await;
        self.terminals.shutdown();
        self.agent_accounts.shutdown();
        self.doc_host.flush_all();
        self.workspace.shutdown();
    }
}

pub struct Engine {
    pub config: EngineConfig,
    device_bootstrap: Option<DeviceBootstrapConfig>,
}

/// A fully assembled identity-scoped engine plus the relay handle whose lifetime
/// keeps this device reachable. Used by both the headless server and the headed
/// in-process engine so their production authentication paths cannot diverge.
pub struct EngineRuntime {
    core: EngineCore,
    _host_relay: Option<comet_rpc::HostRelay>,
}

impl EngineRuntime {
    pub fn core(&self) -> &EngineCore {
        &self.core
    }

    pub async fn shutdown(&self) {
        self.core.shutdown().await;
    }
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config,
            device_bootstrap: None,
        }
    }

    pub fn with_device_bootstrap(mut self, bootstrap: DeviceBootstrapConfig) -> Self {
        self.device_bootstrap = Some(bootstrap);
        self
    }

    /// Resolve Scaffold OAuth or explicit local development authentication.
    pub async fn build_auth(config: &EngineConfig) -> Auth {
        Self::build_auth_for(config, None).await
    }

    async fn build_auth_for(
        config: &EngineConfig,
        bootstrap: Option<&DeviceBootstrapConfig>,
    ) -> Auth {
        let mut auth_config = AuthConfig::new(config.edge_url.clone(), config.data_dir.clone());
        auth_config.scaffold_url = config.scaffold_url.clone();
        auth_config.project_scope = bootstrap
            .map(|value| value.project_id.clone())
            .unwrap_or_else(|| config.project_scope.clone());
        if let Some(bootstrap) = bootstrap {
            auth_config.device_join_grant = Some(bootstrap.device_join_grant.clone());
            auth_config.expected_device_id = Some(bootstrap.device_id.clone());
            auth_config.expected_session_id = Some(bootstrap.session_id.clone());
            auth_config.expected_deployment_id = Some(bootstrap.deployment_id.clone());
            auth_config.expected_lifecycle_epoch = Some(bootstrap.lifecycle_epoch);
            auth_config.expected_sandbox_id = Some(bootstrap.sandbox_id.clone());
        }
        if let Ok(scopes) = std::env::var("COMET_OAUTH_SCOPES")
            && !scopes.trim().is_empty()
        {
            auth_config.oauth_scopes = scopes;
        }
        if let Ok(capabilities) = std::env::var("COMET_SESSION_CAPABILITIES")
            && !capabilities.trim().is_empty()
        {
            auth_config.internal_capabilities = capabilities;
        }
        auth_config.callback_port = Some(
            std::env::var("COMET_CALLBACK_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(27641),
        );
        if let Some(token) = &config.edge_token {
            auth_config.dev_user_id = token.clone();
        }
        Auth::detect(auth_config).await
    }

    /// Open the identity-scoped stores and online transports for an auth session
    /// that is already ready. The headed UI waits behind its sign-in gate before
    /// calling this; headless mode waits on the terminal flow.
    pub async fn assemble_runtime(
        config: &EngineConfig,
        auth: Auth,
    ) -> anyhow::Result<EngineRuntime> {
        let online = (auth.oauth_enabled() || auth.device_mode() || config.edge_token.is_some())
            && auth.access_token().await.is_some();
        let device_id = load_or_create_device_id(&config.data_dir)?;
        let edge = online.then(|| {
            let edge = EdgeConfig::new(config.edge_url.clone(), Arc::new(auth.clone()))
                .with_device(device_id);
            match config.deployment_id.as_deref() {
                Some(deployment_id) => edge.with_deployment(deployment_id),
                None => edge,
            }
        });

        let project_scope = auth
            .state()
            .project_scope()
            .map(str::to_string)
            .unwrap_or_else(|| config.project_scope.clone());
        let user_id = auth
            .user_id()
            .unwrap_or_else(|| env_or("COMET_USER_ID", DEFAULT_USER_ID));
        let core = EngineCore::assemble_with_identity_and_ipc_port(
            &config.data_dir,
            Arc::new(default_registry(config.runtime_profile)),
            config.default_harness,
            edge.clone(),
            AssemblyContext {
                project_scope: &project_scope,
                user_id: &user_id,
                runtime_profile: config.runtime_profile,
                ipc_port: config.ipc_port,
            },
        )?;
        core.set_auth(auth.clone());
        if let Some(scaffold_url) = config.scaffold_url.as_deref()
            && !auth.device_mode()
            && config.runtime_profile.allows_scaffold_control()
        {
            let bearer: Arc<dyn comet_rpc::TokenSource> = Arc::new(auth.clone());
            let client = ScaffoldClient::new(scaffold_url, project_scope.clone(), bearer.clone())?;
            core.sessions
                .set_inference_relay(inference_relay::InferenceRelay::start(client.clone())?);
            let grants = Arc::new(EdgeDeviceJoinGrantClient::new(&config.edge_url, bearer)?);
            core.set_scaffold_runtime(ScaffoldRuntime::new(
                client,
                config.edge_url.clone(),
                grants,
            ));
        }
        // Release checker: polls {edge}/releases on a 6h cadence; headless
        // installs with COMET_AUTO_UPDATE=1 apply + restart themselves — gated
        // on quiescence so a restart never lands under a live run or open PTY.
        let quiescent: comet_update::QuiescentCheck = {
            let sessions = core.sessions.clone();
            let terminals = core.terminals.clone();
            Arc::new(move || !sessions.any_active() && !terminals.any_open())
        };
        let update_access_token: comet_update::AccessTokenSource = {
            let auth = auth.clone();
            Arc::new(move || {
                let auth = auth.clone();
                Box::pin(async move { auth.access_token().await })
            })
        };
        core.set_updater(comet_update::Updater::spawn(
            config.edge_url.clone(),
            config.data_dir.clone(),
            Some(quiescent),
            update_access_token,
            harness_update_specs(),
        ));
        // The "on update" contract: the first boot of a NEW Comet version
        // refreshes the local agent CLIs through their own updaters (opt out
        // via the Settings toggle or COMET_UPDATE_HARNESSES=0). Boot-time, not
        // restart-time — every apply path (bundle swap, symlink swap + service
        // restart, installer re-run) funnels through exactly one first boot.
        if comet_update::version_transition(&config.data_dir)
            && let Some(updater) = core.updater()
            && updater.harness_auto_update()
        {
            updater.spawn_post_update_refresh();
        }
        tracing::info!(device_id = %core.device_id, "engine core assembled");

        let host_relay = edge.as_ref().map(|edge| {
            let links = comet_rpc::LinkCache::new(comet_rpc::LinkCacheConfig::new(
                edge.url.clone(),
                Arc::new(auth.clone()),
            ));
            let links_for_presence = links.clone();
            core.workspace
                .set_peer_alive_hook(Arc::new(move |device_id: &str| {
                    links_for_presence.reset_cooldown(device_id);
                }));
            core.set_links(links);
            core.start_host_relay(&edge.url)
        });

        Ok(EngineRuntime {
            core,
            _host_relay: host_relay,
        })
    }

    /// Run until ctrl-c: auth, sessions engine + doc host + command executor,
    /// IPC server, and, when edge+auth are ready, device routing.
    pub async fn run(self) -> anyhow::Result<()> {
        let config = self.config;
        let bootstrap = self.device_bootstrap;
        tracing::info!(data_dir = %config.data_dir.display(), "engine starting");

        std::fs::create_dir_all(&config.data_dir)?;
        if let Some(bootstrap) = &bootstrap {
            validate_device_bootstrap(&config, bootstrap)?;
            persist_expected_device_id(&config.data_dir, &bootstrap.device_id)?;
        }
        let auth = Self::build_auth_for(&config, bootstrap.as_ref()).await;
        if auth.device_mode() && auth.access_token().await.is_none() {
            anyhow::bail!("device_join_grant_unavailable");
        }
        let _auth_task = auth.spawn_refresh_loop();

        // Service managers fail fast instead of waiting on an invisible prompt.
        if auth.oauth_enabled() {
            terminal_sign_in(&auth).await?;
        }

        let runtime = Self::assemble_runtime(&config, auth).await?;

        // A daemon exists to serve this port, so a bind failure is fatal here —
        // unlike the headed app, which can still work over its in-process
        // transport (see `serve_ipc`).
        let server = serve_ipc(config.ipc_port, runtime.core().rpc_service()).await?;

        shutdown_signal().await?;
        tracing::info!("shutting down");
        server.abort();
        runtime.shutdown().await;
        Ok(())
    }
}

/// The agent CLIs the release checker tracks: resolution through
/// `comet-harness`'s candidate chains (login-shell PATH, version-manager
/// bins), "latest" from each vendor's public channel, applies via each CLI's
/// own self-updater — Comet never swaps a vendor binary itself.
fn harness_update_specs() -> Vec<comet_update::HarnessSpec> {
    use comet_update::{HarnessSpec, ReleaseChannel};
    vec![
        HarnessSpec {
            id: "omp",
            name: "OMP",
            resolve: Arc::new(comet_harness::omp::installed_executable),
            channel: ReleaseChannel::GitHub {
                repo: "can1357/oh-my-pi",
            },
            self_update_args: &["update"],
            min_version: Some(comet_harness::omp::MIN_OMP_VERSION),
        },
        HarnessSpec {
            id: "claude-code",
            name: "Claude Code",
            resolve: Arc::new(comet_harness::claude::installed_executable),
            channel: ReleaseChannel::Npm {
                package: "@anthropic-ai/claude-code",
            },
            self_update_args: &["update"],
            min_version: None,
        },
        HarnessSpec {
            id: "codex",
            name: "Codex",
            resolve: Arc::new(comet_harness::codex::installed_executable),
            channel: ReleaseChannel::Npm {
                package: "@openai/codex",
            },
            self_update_args: &["update"],
            min_version: None,
        },
    ]
}

/// Ctrl-C or SIGTERM. systemd/launchd stop (and the auto-updater's service
/// restart) deliver SIGTERM — without catching it the daemon dies mid-write
/// and every stop takes the crash-recovery path instead of the graceful drain.
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Serve the typed RPC on the localhost IPC port.
///
/// Both engines call this: the headless daemon, and the headed app's embedded
/// engine. That second case is the point — an embedded engine that keeps the
/// port to itself forces anyone wanting a second viewport (the terminal app) to
/// stop the desktop app, start a daemon, and start it again in the right order.
/// Serving here means any viewport can just attach.
///
/// Localhost only, exactly as before: this widens *which process* can serve the
/// port, not who can reach it.
pub async fn serve_ipc(
    port: u16,
    service: std::sync::Arc<dyn comet_rpc::RpcService>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "IPC server listening");
    Ok(tokio::spawn(comet_rpc::serve_ws_listener(
        listener, service,
    )))
}

/// Block until the Scaffold OAuth session is signed in. A TTY prints the
/// browser URL and accepts either the loopback callback URL or `state.code`.
pub async fn terminal_sign_in(auth: &Auth) -> Result<(), EngineError> {
    use std::io::IsTerminal;
    let interactive = std::io::stdin().is_terminal();
    let mut state_rx = auth.watch_state();
    let mut stdin_reader: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        match state_rx.borrow().clone() {
            AuthState::SignedIn {
                user,
                project_scope,
            } => {
                tracing::info!(email = %user.email, project = %project_scope, "auth: session ready");
                break;
            }
            AuthState::SignedOut => {
                if !interactive {
                    return Err(EngineError::Other(
                        "not signed in; run `comet login` on this machine first".into(),
                    ));
                }
                if stdin_reader.is_none() {
                    let url = auth.start_headless_sign_in().await?;
                    println!("Sign in to Ashler Comet:\n\n  {url}\n");
                    println!(
                        "If the browser cannot reach this machine, paste its final localhost URL here."
                    );
                    let auth = auth.clone();
                    stdin_reader = Some(tokio::spawn(async move {
                        loop {
                            let Some(line) = read_stdin_line().await else {
                                return;
                            };
                            let pasted = line.trim();
                            if pasted.is_empty() {
                                continue;
                            }
                            match auth.complete_sign_in(pasted).await {
                                Ok(()) => return,
                                Err(err) => println!("Sign-in failed: {err}"),
                            }
                        }
                    }));
                }
            }
        }
        if state_rx.changed().await.is_err() {
            break;
        }
    }
    if let Some(reader) = stdin_reader {
        reader.abort();
    }
    Ok(())
}

/// One line from stdin (blocking read off the runtime). `None` = stdin closed.
async fn read_stdin_line() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None, // EOF / error
            Ok(_) => Some(line),
        }
    })
    .await
    .ok()
    .flatten()
}

/// Best-effort human name for this device's registry row (hostname).
fn local_device_name() -> String {
    std::env::var("COMET_DEVICE_NAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-device".to_string())
}

/// Trimmed env var or the given default.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn ipc_port_from_env() -> u16 {
    std::env::var("COMET_IPC_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(27654)
}

/// Filesystem-safe project/principal path segment.
fn sanitize_path_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn validate_device_bootstrap(
    config: &EngineConfig,
    bootstrap: &DeviceBootstrapConfig,
) -> Result<(), EngineError> {
    let valid_id = |value: &str| {
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    if !bootstrap.device_join_grant.starts_with("cg1.")
        || bootstrap.project_id != config.project_scope
        || !valid_id(&bootstrap.project_id)
        || !valid_id(&bootstrap.deployment_id)
        || !valid_id(&bootstrap.session_id)
        || !valid_id(&bootstrap.device_id)
        || !valid_id(&bootstrap.sandbox_id)
        || bootstrap.lifecycle_epoch == 0
        || bootstrap.device_id
            != format!(
                "comet-scaffold-{}-e{}",
                bootstrap.sandbox_id, bootstrap.lifecycle_epoch
            )
    {
        return Err(EngineError::Other("device_join_grant_unavailable".into()));
    }
    Ok(())
}

fn persist_expected_device_id(data_dir: &Path, expected: &str) -> Result<(), EngineError> {
    let path = data_dir.join("device-id");
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing.trim() == expected => Ok(()),
        Ok(existing) if existing.trim().is_empty() => {
            std::fs::write(path, expected)?;
            Ok(())
        }
        Ok(_) => Err(EngineError::Other(
            "sandbox device id does not match its join grant".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(path, expected)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    let path = data_dir.join("device-id");
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => Ok(id.trim().to_string()),
        Ok(_) | Err(_) => {
            let id = new_id();
            std::fs::write(&path, &id)?;
            Ok(id)
        }
    }
}
