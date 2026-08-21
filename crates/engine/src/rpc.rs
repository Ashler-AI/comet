//! EngineRpc — the engine-side `RpcService`: sessions + docs + the workspace-doc
//! entity surface.
//!
//! Methods (feature-inventory §2):
//! - `ListHarnesses` → `[HarnessDescriptor]`
//! - `ListModels {harness}` → `[Model]`
//! - `QueueCommand {chatId, command}` → `{commandId}` (durable doc command)
//! - `WatchDocMessages {chatId}` → stream of joined `SessionMessageEntry[]`,
//!   re-emitted on every doc change
//! - `ListLocalSessions` → metadata-only recent Claude Code, Codex, OMP,
//!   Prime Agent, and OpenCode histories; `AttachLocalSession {candidateId}`
//!   imports one transcript idempotently
//! - `WatchCollaboration {chatId}` → typed versioned sessions, provenance,
//!   publications, participants, principal, and verified grants
//! - `WatchChats` / `WatchDevices` → streams of the workspace doc's entity rows
//! - `WatchSessions` → stream of `Session[]`: this engine's live statuses merged with
//!   remote devices' workspace session rows
//! - `Mutate {op, …}` → `{ok}` — workspace entity mutations (createChat, renameChat,
//!   setChatArchived, deleteChat, renameDevice, markChatSeen, markChatUnread)
//! - `LocalDevice` → `{deviceId}` — this engine's identity (never forwarded)
//! - AuthRpc: `AuthStatus` (stream), `SignIn`/`SignInHeadless` → `{url}`,
//!   `CompleteSignIn {code}`, and `SignOut`
//! - Repos (§3.5): `ListRepos`, `AddRepo {path}`, `CloneRepo {url}`,
//!   `CreateRepo {name}`, `ListBranches {repoPath}` (default branch first),
//!   `ListFolders {path?}`, `CreateWorktree {repoPath, branch}`, `DeleteWorktree
//!   {repoPath, worktreePath}`, `ReadCheckoutDiff {checkoutId, checksum}` → exact
//!   bounded patch; `WatchCheckoutDiffs` → summary-only `CheckoutDiffSummary[]`
//! - Terminals (§3.4): `OpenTerminal {chatId, cols, rows}` → `TerminalSession`,
//!   `SubscribeTerminal {terminalId, afterSeq?}` → stream of `TerminalEvent`
//!   (replay then live tail), `WriteTerminal {terminalId, data}`, `ResizeTerminal`,
//!   `CloseTerminal`. M5 is single-user local: per-user owner checks land with
//!   real multi-account auth in M6.
//! - Shared Agent Auth accounts: `ListAgentAccounts` → `AgentAccountsSnapshot`,
//!   `MigrateAgentAccount {harness, accountId}` / `RevokeAgentAccount
//!   {accountId}` → snapshot, plus the add-account login flow, and owner-scoped
//!   `GetAgentRouteReceipt {logicalSessionId}` → attribution-only receipt.
//! - Uploads (§3.7): `UploadChunk {uploadId, data, seq?}`,
//!   `UploadCommit {uploadId, fileName}` → `{path}`,
//!   `ReadAttachmentChunk {path, offset}` → `{name, mimeType, data, nextOffset,
//!   done}` (path-jailed to the uploads dir + workspace-known chat cwds).
//!
//! ## Device-addressed routing (`targetDeviceId`, feature-inventory §2.1)
//!
//! ControlRpc methods are relay-forwardable: params may carry `targetDeviceId`. When it
//! names another device, the call is forwarded verbatim over that device's relay DO via
//! the [`LinkCache`] — the remote engine sees its own id and handles locally, so the
//! forward can never loop. Streaming methods are proxied by re-subscribing remotely and
//! piping items. To make another method device-addressable, nothing per-method is needed
//! beyond listing it in [`forwardable`] (and [`is_stream_method`] if it streams);
//! handlers stay transport-agnostic. `QueueCommand` is intentionally local so
//! the caller durably appends before any remote delivery is attempted.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;
use tokio::sync::watch;

use comet_doc::{MessagePart, SessionCommandPayload};
use comet_proto::{
    AgentSessionRecord, Chat, ChatConfig, CollaborationPrincipal, CollaborationScope,
    CollaborationSnapshot, HarnessId, OmpAdvisorSyncBacklog, ParticipantPresence, ParticipantState,
    RuntimeProfile, ScaffoldEnvironmentControl, ScaffoldEnvironmentControlResult,
    SessionEnvironmentSource, SessionRoomProjection, SessionStatus, ToolCall,
    WorktreeDeletionStage,
};
use comet_rpc::{
    GetAgentRouteReceiptParams, LinkCache, PeerMessageResult, PeerReplyResult, PeerWaitResult,
    ReadCheckoutDiffParams, ReadCheckoutDiffResult, RemoveSessionRefResult, ReplyPeerMessageParams,
    RpcError, RpcReply, RpcService, SendPeerMessageParams, SessionRefParams, WaitPeerReplyParams,
    methods, parse_params,
};

use crate::agent_accounts::AgentAccounts;
use crate::auth::Auth;
use crate::diff_sync::CheckoutDiffSync;
use crate::doc_host::DocHost;
use crate::registry::HarnessRegistry;
use crate::repos::{Repos, home_dir};
use crate::scaffold::ScaffoldRuntime;
use crate::sessions::{PeerReply, SessionsEngine};
use crate::terminals::Terminals;
use crate::uploads::Uploads;
use crate::workspace_host::WorkspaceHost;

const FILE_SEARCH_RPC_TIMEOUT: Duration = Duration::from_secs(6);
const SCAFFOLD_OWNER_ROOM_READY_TIMEOUT: Duration = Duration::from_secs(10);
const FILE_SEARCH_FEATURED_PATHS: usize = 32;
const DEFAULT_PEER_WAIT_MS: u64 = 30_000;
const MAX_PEER_WAIT_MS: u64 = 120_000;
const WORKTREE_DELETION_GRACE_DAYS: i64 = 7;

fn peer_timeout(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(
        timeout_ms
            .unwrap_or(DEFAULT_PEER_WAIT_MS)
            .min(MAX_PEER_WAIT_MS),
    )
}

fn worktree_deletion_deadline(now: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    now + chrono::Duration::days(WORKTREE_DELETION_GRACE_DAYS)
}

fn canonical_session_id(value: &str) -> Option<String> {
    uuid::Uuid::parse_str(value).ok().map(|id| id.to_string())
}

fn peer_reply_result(reply: PeerReply) -> PeerReplyResult {
    PeerReplyResult {
        command_id: reply.command_id,
        text: reply.text,
        source_chat_id: reply.source_chat_id,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatParams {
    chat_id: String,
    #[serde(default)]
    room_projection: Option<SessionRoomProjection>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadDocMessagesParams {
    chat_id: String,
    before: usize,
    #[serde(default)]
    room_projection: Option<SessionRoomProjection>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListModelsParams {
    harness: HarnessId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListHarnessCommandsParams {
    harness: HarnessId,
    #[serde(default)]
    cwd: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachLocalSessionParams {
    candidate_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaptureOmpSessionArtifactParams {
    candidate_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OmpAdvisorConfigParams {
    #[serde(default)]
    cwd: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "setting", content = "value", rename_all = "camelCase")]
enum OmpAdvisorConfigSetting {
    Enabled(bool),
    Model(String),
    Subagents(bool),
    SyncBacklog(OmpAdvisorSyncBacklog),
    ImmuneTurns(u32),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetOmpAdvisorConfigParams {
    #[serde(default)]
    cwd: String,
    #[serde(flatten)]
    setting: OmpAdvisorConfigSetting,
}

fn generic_catalog_allowed(profile: RuntimeProfile, method: &str) -> bool {
    profile != RuntimeProfile::ScaffoldHost
        || !matches!(
            method,
            methods::LIST_HARNESSES | methods::LIST_MODELS | methods::LIST_HARNESS_COMMANDS
        )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueCommandParams {
    chat_id: String,
    command: SessionCommandPayload,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshScaffoldEnvironmentsParams {
    scope: CollaborationScope,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoPathParams {
    /// `repoPath` per §3.5 (the §2.1 shorthand `repo` is accepted as an alias).
    #[serde(alias = "repo")]
    repo_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwitchRefParams {
    /// The checkout to switch — a session's cwd (main folder or worktree).
    repo_path: String,
    ref_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorktreeParams {
    #[serde(alias = "repo")]
    repo_path: String,
    branch: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteWorktreeParams {
    #[serde(alias = "repo")]
    repo_path: String,
    #[serde(alias = "path")]
    worktree_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListFoldersParams {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileSearchParams {
    query: String,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    /// Existing linked worktree selected for a new chat. The engine accepts it
    /// only after verifying it against the space repository's worktree list.
    #[serde(default)]
    path: Option<String>,
}

fn tool_file_path(call: &ToolCall) -> Option<&str> {
    match call {
        ToolCall::ReadFile { path }
        | ToolCall::WriteFile { path, .. }
        | ToolCall::EditFile { path, .. } => Some(path),
        ToolCall::ApplyPatch { path } | ToolCall::Search { path, .. } => path.as_deref(),
        ToolCall::Exec { .. }
        | ToolCall::Glob { .. }
        | ToolCall::WebFetch { .. }
        | ToolCall::WebSearch { .. }
        | ToolCall::Todo { .. }
        | ToolCall::Agent { .. }
        | ToolCall::Mcp { .. }
        | ToolCall::Unknown { .. } => None,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenTerminalParams {
    chat_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalIdParams {
    terminal_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscribeTerminalParams {
    terminal_id: String,
    #[serde(default)]
    after_seq: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteTerminalParams {
    terminal_id: String,
    /// Base64 input bytes (plain UTF-8 accepted leniently).
    data: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResizeTerminalParams {
    terminal_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MigrateAgentAccountParams {
    harness: HarnessId,
    account_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevokeAgentAccountParams {
    account_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartAgentLoginParams {
    harness: HarnessId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateHarnessParams {
    /// Updater spec id ("omp" | "claude-code" | "codex"), as reported in
    /// `UpdateStatus.harnesses`.
    harness: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetHarnessAutoUpdateParams {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginIdParams {
    login_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompleteAgentLoginParams {
    login_id: String,
    code: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadChunkParams {
    upload_id: String,
    /// Base64 payload chunk.
    data: String,
    #[serde(default)]
    seq: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadCommitParams {
    upload_id: String,
    file_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadAttachmentChunkParams {
    path: String,
    #[serde(default)]
    offset: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallDetailParams {
    chat_id: String,
    tool_id: String,
}

/// The Mutate surface (feature-inventory §2 DataRpc), tagged by `op`.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum MutateParams {
    #[serde(rename_all = "camelCase")]
    CreateChat {
        chat_id: String,
        /// The space the chat is created in — fixes host device + base cwd.
        space_id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        config: Option<ChatConfig>,
        /// The picked ref, named on the row from the first frame (the footer
        /// read "Select ref" until the diff reconciler stamped it).
        #[serde(default)]
        branch: Option<String>,
        /// Cwd override (isolated-worktree path); default = the space's folder.
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Create a space (device + folder pair). Idempotent by id; a live
    /// duplicate `(deviceId, path)` no-ops. `gitDetected` is seeded from the
    /// picker's FolderEntry — the owning device's SpacesSync re-verifies.
    #[serde(rename_all = "camelCase")]
    CreateSpace {
        space_id: String,
        device_id: String,
        path: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        git_detected: bool,
    },
    /// LWW display-name set; `name: None` clears back to basename(path).
    #[serde(rename_all = "camelCase")]
    RenameSpace {
        space_id: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Hard delete: cascades to every chat (and session row) in the space.
    /// Live runs hosted here are interrupted best-effort.
    #[serde(rename_all = "camelCase")]
    DeleteSpace { space_id: String },
    #[serde(rename_all = "camelCase")]
    RenameChat { chat_id: String, title: String },
    /// Set the chat's checkout branch label — the sidebar's
    /// "project · branch" sub-line.
    #[serde(rename_all = "camelCase")]
    SetChatBranch { chat_id: String, branch: String },
    /// Retarget a chat onto another folder — mid-session switch to an
    /// EXISTING worktree (the picked ref's checkout). Next run starts a
    /// fresh harness conversation there (resume is cwd-scoped).
    #[serde(rename_all = "camelCase")]
    SetChatCwd { chat_id: String, cwd: String },
    /// Backdate a chat's activity timestamps (epoch ms) — the sidebar's
    /// relative-time column. Used by tooling/seeds; the doc fold sets these on
    /// real message traffic.
    #[serde(rename_all = "camelCase")]
    SetChatActivity {
        chat_id: String,
        #[serde(default)]
        last_message_at: Option<i64>,
        #[serde(default)]
        created_at: Option<i64>,
    },
    /// Re-home a chat to another device (tooling/seeds; device migration later).
    #[serde(rename_all = "camelCase")]
    SetChatHost { chat_id: String, device_id: String },
    #[serde(rename_all = "camelCase")]
    SetChatArchived { chat_id: String, archived: bool },
    /// Full-config replace on the chat row (comet `SetChatConfig`): the
    /// composer's mid-session model / reasoning / options changes, LWW-synced
    /// so they survive restarts and reach every device.
    #[serde(rename_all = "camelCase")]
    SetChatConfig { chat_id: String, config: ChatConfig },
    /// Tombstone: removes the chats-map row; the session doc remains.
    #[serde(rename_all = "camelCase")]
    DeleteChat { chat_id: String },
    #[serde(rename_all = "camelCase")]
    RenameDevice { device_id: String, name: String },
    /// Synced seen marker (LWW + monotonic guard): clears the "completed"
    /// badge on every device. `at` is epoch ms; default = now.
    #[serde(rename_all = "camelCase")]
    MarkChatSeen {
        chat_id: String,
        #[serde(default)]
        at: Option<i64>,
    },
    /// Clear the seen marker — "mark as unread" raises the attention badge
    /// on every device.
    #[serde(rename_all = "camelCase")]
    MarkChatUnread { chat_id: String },
}

pub struct EngineRpc {
    sessions: SessionsEngine,
    doc_host: DocHost,
    workspace: WorkspaceHost,
    registry: std::sync::Arc<HarnessRegistry>,
    repos: Repos,
    terminals: Terminals,
    diff_sync: CheckoutDiffSync,
    uploads: Uploads,
    agent_accounts: AgentAccounts,
    auth: Option<Auth>,
    links: Option<std::sync::Arc<LinkCache>>,
    updater: Option<comet_update::Updater>,
    scaffold: Option<ScaffoldRuntime>,
    runtime_profile: RuntimeProfile,
}

impl EngineRpc {
    #[allow(clippy::too_many_arguments)] // engine assembly seam, not a public API
    pub fn new(
        sessions: SessionsEngine,
        doc_host: DocHost,
        workspace: WorkspaceHost,
        registry: std::sync::Arc<HarnessRegistry>,
        repos: Repos,
        terminals: Terminals,
        diff_sync: CheckoutDiffSync,
        uploads: Uploads,
        agent_accounts: AgentAccounts,
        runtime_profile: RuntimeProfile,
    ) -> Self {
        Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            diff_sync,
            uploads,
            agent_accounts,
            auth: None,
            links: None,
            updater: None,
            scaffold: None,
            runtime_profile,
        }
    }

    /// Attach the auth service (AuthStatus + AuthRpc mutations).
    pub fn with_auth(mut self, auth: Auth) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Attach the peer link cache — enables `targetDeviceId` relay forwarding.
    pub fn with_links(mut self, links: std::sync::Arc<LinkCache>) -> Self {
        self.links = Some(links);
        self
    }

    /// Attach the release checker (UpdateStatus stream + ApplyUpdate).
    pub fn with_updater(mut self, updater: comet_update::Updater) -> Self {
        self.updater = Some(updater);
        self
    }

    /// Attach the native Scaffold control plane and its event-driven watch.
    pub fn with_scaffold(mut self, scaffold: ScaffoldRuntime) -> Self {
        self.scaffold = Some(scaffold);
        self
    }

    fn auth(&self) -> Result<&Auth, RpcError> {
        self.auth
            .as_ref()
            .ok_or_else(|| RpcError::Failed("auth unavailable".into()))
    }

    fn updater(&self) -> Result<&comet_update::Updater, RpcError> {
        self.updater
            .as_ref()
            .ok_or_else(|| RpcError::Failed("updates unavailable".into()))
    }

    fn scaffold(&self) -> Result<&ScaffoldRuntime, RpcError> {
        if !self.runtime_profile.allows_scaffold_control() {
            return Err(RpcError::Failed(
                "scaffold_control_disabled_by_runtime_profile".into(),
            ));
        }
        self.scaffold
            .as_ref()
            .ok_or_else(|| RpcError::Failed("scaffold_control_plane_unavailable".into()))
    }

    fn prepare_scaffold_attach(
        &self,
        control: &ScaffoldEnvironmentControl,
    ) -> Result<Option<std::sync::Arc<crate::doc_host::ChatDocHandle>>, RpcError> {
        let ScaffoldEnvironmentControl::Attach { scope, .. } = control else {
            return Ok(None);
        };
        let auth_state = self.auth()?.state();
        let project_scope = auth_state
            .project_scope()
            .ok_or_else(|| RpcError::Failed("authenticated project scope unavailable".into()))?;
        if scope.project_id != project_scope || scope.project_id != self.workspace.project_scope() {
            return Err(RpcError::Failed("Scaffold attach project mismatch".into()));
        }
        let Some(deployment_id) = scope
            .deployment_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        let Some(session_id) = scope
            .session_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        let projection = SessionRoomProjection {
            project_id: scope.project_id.clone(),
            deployment_id: deployment_id.to_string(),
            session_id: session_id.to_string(),
        };
        self.doc_host
            .open_projection(session_id, Some(&projection))
            .map(Some)
            .map_err(|error| RpcError::Failed(error.to_string()))
    }

    async fn await_scaffold_owner_room(
        &self,
        handle: Option<&crate::doc_host::ChatDocHandle>,
        cancellation: &comet_harness::CancellationToken,
    ) -> Result<(), RpcError> {
        let Some(handle) = handle else {
            return Ok(());
        };
        tokio::time::timeout(SCAFFOLD_OWNER_ROOM_READY_TIMEOUT, async {
            while !handle.connected() {
                tokio::select! {
                    _ = cancellation.cancelled() => {
                        return Err(RpcError::Failed("scaffold_request_cancelled".into()));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| RpcError::Failed("scaffold_session_owner_room_unavailable".into()))?
    }

    fn require_agent_accounts(&self) -> Result<(), RpcError> {
        if self.runtime_profile.allows_agent_accounts() {
            Ok(())
        } else {
            Err(RpcError::Failed(
                "agent_accounts_disabled_by_runtime_profile".into(),
            ))
        }
    }

    fn require_session_import(&self) -> Result<(), RpcError> {
        if self.runtime_profile.allows_session_import() {
            Ok(())
        } else {
            Err(RpcError::Failed(
                "local_session_import_disabled_by_runtime_profile".into(),
            ))
        }
    }

    /// Local owner authority is derived from the attached authenticated identity.
    /// The UI supplies no capability list; the one required capability is selected
    /// from the typed action and persisted as a short-lived verified grant.
    fn install_local_owner_grant(&self, command: &SessionCommandPayload) -> Result<(), RpcError> {
        let SessionCommandPayload::Control {
            session_id,
            owner_device_id,
            actor_device_id,
            actor_subject,
            grant_id,
            source,
            action,
        } = command
        else {
            return Ok(());
        };
        let local_device_id = self.doc_host.device_id();
        // Commands for another owner are appended locally and authorized by
        // that owner from its relay-ingested grant. Never synthesize authority
        // for a remote target on the caller's device.
        if actor_device_id != local_device_id || owner_device_id != local_device_id {
            return Ok(());
        }
        let auth = self.auth()?;
        let state = auth.state();
        let user = state
            .user()
            .ok_or_else(|| RpcError::Failed("authenticated local identity unavailable".into()))?;
        let project_scope = state
            .project_scope()
            .ok_or_else(|| RpcError::Failed("authenticated project scope unavailable".into()))?;
        if actor_subject != &user.id
            || !matches!(source, comet_proto::AgentSessionSource::Local)
            || grant_id.trim().is_empty()
            || session_id.trim().is_empty()
        {
            return Err(RpcError::Failed(
                "local session control identity mismatch".into(),
            ));
        }
        let now = crate::now_ms();
        let grant = comet_proto::CapabilityGrant {
            id: grant_id.clone(),
            principal_subject: user.id.clone(),
            scope: CollaborationScope {
                project_id: project_scope.to_string(),
                deployment_id: Some(project_scope.to_string()),
                session_id: Some(session_id.clone()),
                unknown: Default::default(),
            },
            capabilities: vec![action.required_capability().to_string()],
            sandbox_id: None,
            device_id: Some(owner_device_id.clone()),
            lifecycle_epoch: None,
            granted_by: "authenticated-local-identity".into(),
            granted_at: now,
            expires_at: Some(now + crate::doc_host::LOCAL_OWNER_GRANT_TTL_MS),
            revoked_at: None,
            unknown: Default::default(),
        };
        self.doc_host
            .install_local_owner_grant(grant)
            .map_err(|error| RpcError::Failed(error.to_string()))
    }
    /// Project the identifier half of the exact device grant into this
    /// authenticated viewport. The bootstrap secret remains sandbox-only.
    fn install_scaffold_control_grant(
        &self,
        result: &ScaffoldEnvironmentControlResult,
    ) -> Result<(), RpcError> {
        let Some(control_grant) = result.control_grant.as_ref() else {
            return Ok(());
        };
        let Some(attached_device_id) = result.attached_device_id.as_ref() else {
            return Err(RpcError::Failed(
                "Scaffold control grant has no attached device".into(),
            ));
        };
        let SessionEnvironmentSource::Scaffold {
            sandbox_id,
            lifecycle_epoch,
            ..
        } = &result.environment.source
        else {
            return Err(RpcError::Failed(
                "Scaffold control grant has no sandbox".into(),
            ));
        };
        let auth = self.auth()?;
        let state = auth.state();
        let user = state
            .user()
            .ok_or_else(|| RpcError::Failed("authenticated local identity unavailable".into()))?;
        let project_scope = state
            .project_scope()
            .ok_or_else(|| RpcError::Failed("authenticated project scope unavailable".into()))?;
        if result.environment.scope.project_id != project_scope {
            return Err(RpcError::Failed(
                "Scaffold control grant project mismatch".into(),
            ));
        }
        let grant = comet_proto::CapabilityGrant {
            id: control_grant.id.clone(),
            principal_subject: user.id.clone(),
            scope: result.environment.scope.clone(),
            capabilities: control_grant.capabilities.clone(),
            sandbox_id: Some(sandbox_id.clone()),
            device_id: Some(attached_device_id.clone()),
            lifecycle_epoch: *lifecycle_epoch,
            granted_by: "comet-edge-device-room".into(),
            granted_at: crate::now_ms(),
            expires_at: Some(control_grant.expires_at),
            revoked_at: None,
            unknown: Default::default(),
        };
        self.doc_host
            .install_scaffold_control_grant(grant)
            .map_err(|error| RpcError::Failed(error.to_string()))
    }

    /// Resolve a mention-search root from synced workspace rows. A client may
    /// name an existing linked worktree for a new chat, but it is verified
    /// against the space repository before any filesystem walk begins.
    async fn file_search_root(&self, p: &FileSearchParams) -> Result<std::path::PathBuf, RpcError> {
        let local_device = self.doc_host.device_id();
        match (&p.chat_id, &p.space_id) {
            (Some(_), Some(_)) | (None, None) => Err(RpcError::BadParams(
                "SearchFiles needs exactly one of chatId or spaceId".into(),
            )),
            (Some(chat_id), None) => {
                if p.path.is_some() {
                    return Err(RpcError::BadParams(
                        "SearchFiles path applies only to a space".into(),
                    ));
                }
                let chat = self
                    .workspace
                    .doc()
                    .chat(chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("chat not found".into()))?;
                if chat.device_id != local_device {
                    return Err(RpcError::Failed("chat belongs to another device".into()));
                }
                let cwd = chat
                    .cwd
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| RpcError::Failed("chat has no workspace folder".into()))?;
                let space_id = chat
                    .space_id
                    .ok_or_else(|| RpcError::Failed("chat has no workspace space".into()))?;
                let space = self
                    .workspace
                    .doc()
                    .space(&space_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("chat workspace space not found".into()))?;
                if space.device_id != local_device {
                    return Err(RpcError::Failed(
                        "chat space belongs to another device".into(),
                    ));
                }
                if let Some(cwd) = self
                    .repos
                    .workspace_checkout(std::path::Path::new(&space.path), &cwd)
                    .await
                {
                    Ok(cwd)
                } else {
                    Err(RpcError::Failed(
                        "chat folder is not a workspace checkout".into(),
                    ))
                }
            }
            (None, Some(space_id)) => {
                let space = self
                    .workspace
                    .doc()
                    .space(space_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("space not found".into()))?;
                if space.device_id != local_device {
                    return Err(RpcError::Failed("space belongs to another device".into()));
                }
                let space_path = std::path::PathBuf::from(&space.path);
                let requested = p
                    .path
                    .as_deref()
                    .map_or_else(|| space_path.clone(), std::path::PathBuf::from);
                if let Some(requested) =
                    self.repos.workspace_checkout(&space_path, &requested).await
                {
                    Ok(requested)
                } else {
                    Err(RpcError::BadParams(
                        "SearchFiles path is not a workspace checkout".into(),
                    ))
                }
            }
        }
    }

    /// Most-recent-first paths the current chat actually touched, followed by
    /// files still changed in its checkout. The search worker validates and
    /// normalizes them against the resolved root before using them as ranking
    /// hints, so stale or out-of-workspace tool paths simply disappear.
    fn featured_file_paths(&self, chat_id: &str) -> Vec<String> {
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        if let Ok(handle) = self.doc_host.open(chat_id)
            && let Ok(entries) = handle.doc().read_entries()
        {
            for entry in entries.into_iter().rev() {
                for part in entry.parts.into_iter().rev() {
                    if let MessagePart::Tool { call, .. } = part
                        && let Some(path) = tool_file_path(&call)
                        && !path.trim().is_empty()
                        && seen.insert(path.to_string())
                    {
                        paths.push(path.to_string());
                        if paths.len() == FILE_SEARCH_FEATURED_PATHS {
                            break;
                        }
                    }
                }
                if paths.len() == FILE_SEARCH_FEATURED_PATHS {
                    break;
                }
            }
        }

        if let Ok(Some(chat)) = self.workspace.doc().chat(chat_id) {
            let diffs = self.diff_sync.watch_diffs().borrow().clone();
            let diff = chat
                .checkout_id
                .as_deref()
                .and_then(|id| diffs.iter().find(|diff| diff.checkout_id == id))
                .or_else(|| {
                    chat.cwd
                        .as_deref()
                        .and_then(|cwd| diffs.iter().find(|diff| diff.cwd == cwd))
                });
            if let Some(diff) = diff {
                for file in &diff.files {
                    if paths.len() == FILE_SEARCH_FEATURED_PATHS {
                        break;
                    }
                    if seen.insert(file.path.clone()) {
                        paths.push(file.path.clone());
                    }
                }
            }
        }
        paths
    }

    /// Forward a device-addressed call over the target device's relay. On transport
    /// failure the cached link is invalidated so the next call re-dials.
    async fn forward(
        &self,
        target: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let Some(links) = &self.links else {
            return Err(RpcError::Failed(format!(
                "cannot reach device {target}: remote routing unavailable (offline)"
            )));
        };
        let client = links.client(target).await?;
        if is_stream_method(method) {
            let rx = match client.subscribe(method, params).await {
                Ok(rx) => rx,
                Err(err) => {
                    links.invalidate(target);
                    return Err(err);
                }
            };
            // Pipe remote items; the held client keeps the link's RpcClient alive for
            // the stream's lifetime. A remote error just ends the stream (the relay
            // link-down path fails pending calls; stream receivers close).
            let stream = futures::stream::unfold((rx, client), |(mut rx, client)| async move {
                rx.recv().await.map(|item| (item, (rx, client)))
            });
            return Ok(RpcReply::Stream(stream.boxed()));
        }
        match client.call(method, params).await {
            Ok(value) => Ok(RpcReply::Value(value)),
            Err(err) => {
                if matches!(err, RpcError::Closed | RpcError::Transport(_)) {
                    links.invalidate(target);
                }
                Err(err)
            }
        }
    }

    fn worktree_deletion_stage(&self, chat_id: &str) -> Option<WorktreeDeletionStage> {
        let chat = self.workspace.doc().chat(chat_id).ok()??;
        let cwd = chat.cwd.as_deref()?;
        let path = self
            .repos
            .managed_worktree_path(std::path::Path::new(cwd))?;
        let actor_subject = self.auth.as_ref()?.user_id()?;
        let sessions = self
            .doc_host
            .open(chat_id)
            .ok()
            .and_then(|handle| handle.doc().collaboration_snapshot().ok())
            .map(|snapshot| snapshot.sessions)
            .unwrap_or_default();
        if !owner_may_stage_worktree_deletion(
            &actor_subject,
            self.doc_host.device_id(),
            &chat,
            &sessions,
        ) {
            tracing::info!(
                chat = %chat_id,
                actor = %actor_subject,
                owner_device = %chat.device_id,
                "settled session did not stage worktree cleanup: actor is not the worktree owner"
            );
            return None;
        }
        Some(WorktreeDeletionStage {
            chat_id: chat.id,
            path: path.to_string_lossy().into_owned(),
            owner_subject: actor_subject,
            owner_device_id: chat.device_id,
            delete_after: worktree_deletion_deadline(chrono::Utc::now()),
        })
    }

    fn mutate(&self, params: MutateParams) -> Result<(), RpcError> {
        let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
        match params {
            MutateParams::CreateChat {
                chat_id,
                space_id,
                title,
                config,
                branch,
                cwd,
            } => {
                self.workspace
                    .create_chat(&chat_id, &space_id, config, cwd)
                    .map_err(failed)?;
                if let Some(title) = title.as_deref().filter(|title| !title.is_empty()) {
                    self.workspace
                        .rename_chat(&chat_id, title)
                        .map_err(failed)?;
                }
                if let Some(branch) = branch.as_deref().filter(|b| !b.is_empty()) {
                    self.workspace
                        .set_chat_branch(&chat_id, branch)
                        .map_err(failed)?;
                }
                Ok(())
            }
            MutateParams::CreateSpace {
                space_id,
                device_id,
                path,
                name,
                git_detected,
            } => self
                .workspace
                .create_space(&space_id, &device_id, &path, name, git_detected)
                .map_err(failed),
            MutateParams::RenameSpace { space_id, name } => self
                .workspace
                .rename_space(&space_id, name.as_deref())
                .map_err(failed)
                .map(drop),
            MutateParams::DeleteSpace { space_id } => {
                let deleted = self.workspace.delete_space(&space_id).map_err(failed)?;
                // Best-effort teardown of live runs we host for the deleted chats
                // (the doc rows are already tombstoned; a straggler run would only
                // write into an orphaned session doc).
                let sessions = self.sessions.clone();
                let doc_host = self.doc_host.clone();
                let chat_ids = deleted.chat_ids;
                tokio::spawn(async move {
                    for chat_id in chat_ids {
                        if let Err(err) = sessions.interrupt(&chat_id).await {
                            tracing::debug!(chat = %chat_id, error = %err, "deleteSpace interrupt skipped");
                        }
                        doc_host.purge_chat(&chat_id);
                    }
                });
                Ok(())
            }
            MutateParams::RenameChat { chat_id, title } => self
                .workspace
                .rename_chat(&chat_id, &title)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatBranch { chat_id, branch } => self
                .workspace
                .set_chat_branch(&chat_id, &branch)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatCwd { chat_id, cwd } => self
                .workspace
                .set_chat_cwd(&chat_id, &cwd)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatActivity {
                chat_id,
                last_message_at,
                created_at,
            } => self
                .workspace
                .set_chat_activity(&chat_id, last_message_at, created_at)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatHost { chat_id, device_id } => self
                .workspace
                .set_chat_host(&chat_id, &device_id)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatArchived { chat_id, archived } => {
                let stage = archived
                    .then(|| self.worktree_deletion_stage(&chat_id))
                    .flatten();
                self.workspace
                    .set_chat_archived_with_worktree_deletion(&chat_id, archived, stage.as_ref())
                    .map_err(failed)
                    .map(drop)
            }
            MutateParams::SetChatConfig { chat_id, config } => self
                .workspace
                .set_chat_config(&chat_id, &config)
                .map_err(failed)
                .map(drop),
            MutateParams::DeleteChat { chat_id } => {
                if let Some(stage) = self.worktree_deletion_stage(&chat_id) {
                    self.workspace
                        .set_chat_archived_with_worktree_deletion(&chat_id, true, Some(&stage))
                        .map_err(failed)?;
                }
                self.workspace.delete_chat(&chat_id).map_err(failed)?;
                self.doc_host.purge_chat(&chat_id);
                Ok(())
            }
            MutateParams::RenameDevice { device_id, name } => self
                .workspace
                .rename_device(&device_id, &name)
                .map_err(failed)
                .map(drop),
            MutateParams::MarkChatSeen { chat_id, at } => {
                let at = at
                    .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                    .unwrap_or_else(chrono::Utc::now);
                self.workspace
                    .mark_chat_seen(&chat_id, at)
                    .map_err(failed)
                    .map(drop)
            }
            MutateParams::MarkChatUnread { chat_id } => self
                .workspace
                .mark_chat_unread(&chat_id)
                .map_err(failed)
                .map(drop),
        }
    }
}

fn owner_may_stage_worktree_deletion(
    actor_subject: &str,
    local_device_id: &str,
    chat: &Chat,
    sessions: &[AgentSessionRecord],
) -> bool {
    if chat.device_id != local_device_id {
        return false;
    }
    sessions.is_empty()
        || sessions.iter().any(|session| {
            session.owner_subject == actor_subject && session.owner_device_id == local_device_id
        })
}

/// ControlRpc methods that operate on device-local resources and therefore
/// honor `targetDeviceId`. Durable session commands are deliberately excluded:
/// the authenticated caller's engine must append them to its local shared
/// document first, then the document host drains them after sync.
fn forwardable(method: &str) -> bool {
    matches!(
        method,
        methods::LIST_HARNESSES
            | methods::LIST_MODELS
            | methods::LIST_HARNESS_COMMANDS
            | methods::TAKE_OVER_OMP_SESSION
            | methods::WATCH_DOC_MESSAGES
            | methods::READ_DOC_MESSAGES
            | "WatchCollaboration"
            // Repos/worktrees/folders are device-local filesystem state.
            | methods::LIST_REPOS
            | methods::ADD_REPO
            | methods::CLONE_REPO
            | methods::CREATE_REPO
            | methods::LIST_BRANCHES
            | methods::LIST_REFS
            | methods::SWITCH_REF
            | methods::LIST_FOLDERS
            | methods::SEARCH_FILES
            | methods::CREATE_WORKTREE
            | methods::DELETE_WORKTREE
            // Checkout diffs are produced on the device holding the checkout.
            | methods::WATCH_CHECKOUT_DIFFS
            | methods::READ_CHECKOUT_DIFF
            // Terminals live on the chat's host device.
            | methods::OPEN_TERMINAL
            | methods::SUBSCRIBE_TERMINAL
            | methods::WRITE_TERMINAL
            | methods::RESIZE_TERMINAL
            | methods::CLOSE_TERMINAL
            // Uploads/attachments target the chat's host device (the agent reads
            // the committed file from that device's disk).
            | methods::UPLOAD_CHUNK
            | methods::UPLOAD_COMMIT
            | methods::READ_ATTACHMENT_CHUNK
            // Tool details come from the run journal on the device that ran
            // the turn.
            | methods::TOOL_CALL_DETAIL
            // Updates report/apply on the device whose binary they concern.
            | methods::UPDATE_STATUS
            | methods::APPLY_UPDATE
            | methods::STAGE_UPDATE
            // Agent-CLI updates and their auto-refresh pref are per-device too.
            | methods::UPDATE_HARNESS
            | methods::SET_HARNESS_AUTO_UPDATE
    )
}

/// Forwardable methods whose reply is a stream (proxied item-by-item).
fn is_stream_method(method: &str) -> bool {
    matches!(
        method,
        methods::WATCH_DOC_MESSAGES
            | "WatchCollaboration"
            | methods::SUBSCRIBE_TERMINAL
            | methods::WATCH_CHECKOUT_DIFFS
            | methods::UPDATE_STATUS
    )
}

/// A watch receiver as a stream: current value first, then every change.
fn watch_stream<T>(rx: watch::Receiver<T>) -> BoxStream<'static, serde_json::Value>
where
    T: serde::Serialize + Clone + Send + Sync + 'static,
{
    futures::stream::unfold((rx, false), |(mut rx, emitted)| async move {
        if emitted {
            rx.changed().await.ok()?;
        }
        let value = {
            let borrowed = rx.borrow_and_update();
            serde_json::to_value(&*borrowed).ok()?
        };
        Some((value, (rx, true)))
    })
    .boxed()
}

/// The transcript watch as delta frames (`comet_doc::transcript_delta`): a
/// full `reset` first, then only changed entries per commit — the whole-Vec
/// serialization here was the per-tick cost that scaled with transcript size.
fn doc_messages_stream(
    rx: watch::Receiver<comet_doc::SessionEntryWindow>,
) -> BoxStream<'static, serde_json::Value> {
    use comet_doc::transcript_delta::{TranscriptFrame, diff_transcript};
    futures::stream::unfold(
        (rx, None::<Vec<comet_doc::SessionMessageEntry>>),
        |(mut rx, mut prev)| async move {
            loop {
                if prev.is_some() {
                    rx.changed().await.ok()?;
                }
                let current = rx.borrow_and_update().clone();
                let frame = match prev.as_deref() {
                    None => TranscriptFrame::reset(&current.entries, current.before),
                    Some(prev) => diff_transcript(prev, &current.entries, current.before),
                };
                prev = Some(current.entries);
                // No-op commits (a second watcher attaching, command-only
                // changes) produce empty deltas — skip the frame entirely.
                if frame.is_empty_delta() {
                    continue;
                }
                let value = serde_json::to_value(&frame).ok()?;
                return Some((value, (rx, prev)));
            }
        },
    )
    .boxed()
}

fn project_collaboration_snapshot(
    doc: &comet_doc::SessionDoc,
    doc_host: &DocHost,
    auth: Option<&Auth>,
    room_projection: Option<&SessionRoomProjection>,
) -> Option<CollaborationSnapshot> {
    let mut snapshot = doc.collaboration_snapshot().ok()?;
    let chat_id = doc.chat_id()?;
    comet_doc::reanchor_projected_file_annotations(&mut snapshot, |anchor| {
        doc_host.annotation_target_text(&chat_id, anchor)
    });
    let Some(auth) = auth else {
        return Some(snapshot);
    };
    let state = auth.state();
    let Some(user) = state.user() else {
        return Some(snapshot);
    };
    let project_id = state.project_scope()?.to_string();
    let mut session_ids = snapshot
        .sessions
        .iter()
        .map(|session| session.session_id.clone())
        .collect::<Vec<_>>();
    if let Some(projection) = room_projection
        && !session_ids.contains(&projection.session_id)
    {
        session_ids.push(projection.session_id.clone());
    }
    snapshot.grants = doc_host.collaboration_grants(&user.id, &session_ids);
    if let Some(projection) = room_projection {
        snapshot.grants.retain(|grant| {
            grant.scope.project_id == projection.project_id
                && grant.scope.deployment_id.as_deref() == Some(&projection.deployment_id)
                && grant.scope.session_id.as_deref() == Some(&projection.session_id)
        });
    }
    snapshot.principal = Some(CollaborationPrincipal {
        subject: user.id.clone(),
        email: Some(user.email.clone()),
        project_id,
        deployment_id: room_projection.map(|projection| projection.deployment_id.clone()),
        session_id: room_projection.map(|projection| projection.session_id.clone()),
        capabilities: auth.capabilities(),
        unknown: Default::default(),
    });

    let mut participants = BTreeMap::<String, ParticipantPresence>::new();
    participants.insert(
        comet_proto::participant_presence_key(&user.id, doc_host.device_id()),
        ParticipantPresence {
            principal_subject: user.id.clone(),
            display_name: user.name.clone().or_else(|| Some(user.email.clone())),
            device_id: doc_host.device_id().to_string(),
            state: ParticipantState::Active,
            last_seen_at: crate::now_ms(),
            focused_target_id: None,
            cursor: None,
            unknown: Default::default(),
        },
    );
    for session in &snapshot.sessions {
        let state = match session.status {
            Some(SessionStatus::Working | SessionStatus::AwaitingInput) => ParticipantState::Active,
            Some(SessionStatus::Idle) | None => ParticipantState::Idle,
            Some(SessionStatus::Errored) => ParticipantState::Disconnected,
        };
        let key =
            comet_proto::participant_presence_key(&session.owner_subject, &session.owner_device_id);
        let participant = ParticipantPresence {
            principal_subject: session.owner_subject.clone(),
            display_name: (session.owner_subject == user.id)
                .then(|| user.name.clone().unwrap_or_else(|| user.email.clone())),
            device_id: session.owner_device_id.clone(),
            state,
            last_seen_at: session.updated_at.unwrap_or(session.created_at),
            focused_target_id: Some(session.session_id.clone()),
            cursor: None,
            unknown: Default::default(),
        };
        participants
            .entry(key)
            .and_modify(|current| merge_participant_presence(current, &participant))
            .or_insert(participant);
    }
    snapshot.participants = participants.into_values().collect();
    Some(snapshot)
}

fn merge_participant_presence(current: &mut ParticipantPresence, candidate: &ParticipantPresence) {
    if candidate.last_seen_at >= current.last_seen_at {
        current.last_seen_at = candidate.last_seen_at;
        current.focused_target_id = candidate.focused_target_id.clone();
        current.state = candidate.state;
    }
    if current.display_name.is_none() {
        current.display_name.clone_from(&candidate.display_name);
    }
}

fn collaboration_stream(
    messages_rx: watch::Receiver<comet_doc::SessionEntryWindow>,
    authority_rx: watch::Receiver<u64>,
    doc: std::sync::Arc<comet_doc::SessionDoc>,
    doc_host: DocHost,
    auth: Option<Auth>,
    room_projection: Option<SessionRoomProjection>,
) -> BoxStream<'static, serde_json::Value> {
    let auth_rx = auth.as_ref().map(Auth::watch_state);
    futures::stream::unfold(
        (
            messages_rx,
            authority_rx,
            auth_rx,
            doc,
            doc_host,
            auth,
            room_projection,
            false,
        ),
        |(
            mut messages_rx,
            mut authority_rx,
            mut auth_rx,
            doc,
            doc_host,
            auth,
            room_projection,
            emitted,
        )| async move {
            if emitted {
                tokio::select! {
                    result = messages_rx.changed() => result.ok()?,
                    result = authority_rx.changed() => result.ok()?,
                    result = async {
                        auth_rx
                            .as_mut()
                            .expect("auth branch is guarded")
                            .changed()
                            .await
                    }, if auth_rx.is_some() => result.ok()?,
                }
            }
            let _ = messages_rx.borrow_and_update();
            let _ = authority_rx.borrow_and_update();
            if let Some(rx) = auth_rx.as_mut() {
                let _ = rx.borrow_and_update();
            }
            let snapshot = project_collaboration_snapshot(
                &doc,
                &doc_host,
                auth.as_ref(),
                room_projection.as_ref(),
            )?;
            let value = serde_json::to_value(snapshot).ok()?;
            Some((
                value,
                (
                    messages_rx,
                    authority_rx,
                    auth_rx,
                    doc,
                    doc_host,
                    auth,
                    room_projection,
                    true,
                ),
            ))
        },
    )
    .boxed()
}

/// Authentication-only RPC surface used before identity-scoped stores open.
/// It exposes Scaffold OAuth sign-in without application organization APIs.
#[derive(Clone)]
pub struct AuthRpc {
    auth: Auth,
}

impl AuthRpc {
    pub fn new(auth: Auth) -> Self {
        Self { auth }
    }

    pub fn handles(method: &str) -> bool {
        matches!(
            method,
            methods::AUTH_STATUS
                | methods::SIGN_IN
                | methods::SIGN_IN_HEADLESS
                | methods::COMPLETE_SIGN_IN
                | methods::SIGN_OUT
        )
    }
}

#[async_trait]
impl RpcService for AuthRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::AUTH_STATUS => Ok(RpcReply::Stream(watch_stream(self.auth.watch_state()))),
            methods::SIGN_IN => {
                let url = self
                    .auth
                    .start_sign_in()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "url": url }))
            }
            methods::SIGN_IN_HEADLESS => {
                let url = self
                    .auth
                    .start_headless_sign_in()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "url": url }))
            }
            methods::COMPLETE_SIGN_IN => {
                #[derive(Deserialize)]
                struct P {
                    code: String,
                }
                let p: P = parse_params(params)?;
                self.auth
                    .complete_sign_in(&p.code)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::SIGN_OUT => {
                self.auth.sign_out();
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}

#[async_trait]
impl RpcService for EngineRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        // Device-addressed routing: forward calls that target another device over its
        // relay. The target compares the id to its own, so forwards cannot loop.
        if forwardable(method)
            && let Some(target) = params.get("targetDeviceId").and_then(|v| v.as_str())
            && target != self.doc_host.device_id()
        {
            let target = target.to_string();
            return self.forward(&target, method, params).await;
        }
        if AuthRpc::handles(method) {
            return AuthRpc::new(self.auth()?.clone())
                .handle(method, params)
                .await;
        }
        match method {
            methods::LIST_HARNESSES if !generic_catalog_allowed(self.runtime_profile, method) => {
                Err(RpcError::Failed(
                    "generic_harness_discovery_disabled_by_runtime_profile".into(),
                ))
            }
            methods::LIST_HARNESSES => RpcReply::value(&self.registry.descriptors()),
            methods::LIST_MODELS if !generic_catalog_allowed(self.runtime_profile, method) => Err(
                RpcError::Failed("generic_model_discovery_disabled_by_runtime_profile".into()),
            ),
            methods::LIST_MODELS => {
                // Catalog discovery is a transient RPC against the harness;
                // it never allocates or enters SessionsEngine state.
                let p: ListModelsParams = parse_params(params)?;
                let harness = self
                    .registry
                    .resolve(p.harness)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let models = harness
                    .models()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&models)
            }
            methods::LIST_HARNESS_COMMANDS
                if !generic_catalog_allowed(self.runtime_profile, method) =>
            {
                Err(RpcError::Failed(
                    "generic_command_discovery_disabled_by_runtime_profile".into(),
                ))
            }
            methods::LIST_HARNESS_COMMANDS => {
                let p: ListHarnessCommandsParams = parse_params(params)?;
                let harness = self
                    .registry
                    .resolve(p.harness)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                let commands = harness
                    .commands(&p.cwd)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&commands)
            }
            methods::GET_OMP_ADVISOR_CONFIG => {
                if self.runtime_profile != RuntimeProfile::LocalController {
                    return Err(RpcError::Failed(
                        "omp_advisor_config_disabled_by_runtime_profile".into(),
                    ));
                }
                let p: OmpAdvisorConfigParams = parse_params(params)?;
                let config = comet_harness::omp::read_advisor_config(&p.cwd)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&config)
            }
            methods::SET_OMP_ADVISOR_CONFIG => {
                if self.runtime_profile != RuntimeProfile::LocalController {
                    return Err(RpcError::Failed(
                        "omp_advisor_config_disabled_by_runtime_profile".into(),
                    ));
                }
                let p: SetOmpAdvisorConfigParams = parse_params(params)?;
                let update = match p.setting {
                    OmpAdvisorConfigSetting::Enabled(value) => {
                        comet_harness::omp::AdvisorConfigUpdate::Enabled(value)
                    }
                    OmpAdvisorConfigSetting::Model(value) => {
                        comet_harness::omp::AdvisorConfigUpdate::Model(value)
                    }
                    OmpAdvisorConfigSetting::Subagents(value) => {
                        comet_harness::omp::AdvisorConfigUpdate::Subagents(value)
                    }
                    OmpAdvisorConfigSetting::SyncBacklog(value) => {
                        comet_harness::omp::AdvisorConfigUpdate::SyncBacklog(value)
                    }
                    OmpAdvisorConfigSetting::ImmuneTurns(value) => {
                        comet_harness::omp::AdvisorConfigUpdate::ImmuneTurns(value)
                    }
                };
                let config = comet_harness::omp::update_advisor_config(&p.cwd, update)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&config)
            }
            methods::LIST_LOCAL_SESSIONS => {
                self.require_session_import()?;
                let workspace = self.workspace.clone();
                let candidates =
                    tokio::task::spawn_blocking(move || crate::local_sessions::list(&workspace))
                        .await
                        .map_err(|err| {
                            RpcError::Failed(format!("local session scan failed: {err}"))
                        })?
                        .map_err(|err| RpcError::Failed(err.to_string()))?;
                RpcReply::value(&candidates)
            }
            methods::CAPTURE_OMP_SESSION_ARTIFACT => {
                self.require_session_import()?;
                let p: CaptureOmpSessionArtifactParams = parse_params(params)?;
                let result = tokio::task::spawn_blocking(move || {
                    crate::local_sessions::capture_omp_artifact(&p.candidate_id)
                })
                .await
                .map_err(|err| RpcError::Failed(format!("OMP session capture failed: {err}")))?
                .map_err(|err| RpcError::Failed(err.to_string()))?;
                RpcReply::value(&result)
            }
            methods::ATTACH_LOCAL_SESSION => {
                self.require_session_import()?;
                let p: AttachLocalSessionParams = parse_params(params)?;
                let workspace = self.workspace.clone();
                let doc_host = self.doc_host.clone();
                let result = tokio::task::spawn_blocking(move || {
                    crate::local_sessions::attach(&p.candidate_id, &workspace, &doc_host)
                })
                .await
                .map_err(|err| RpcError::Failed(format!("local session import failed: {err}")))?
                .map_err(|err| RpcError::Failed(err.to_string()))?;
                RpcReply::value(&result)
            }
            methods::QUEUE_COMMAND => {
                let p: QueueCommandParams = parse_params(params)?;
                let activates_chat = match &p.command {
                    SessionCommandPayload::Run { .. }
                    | SessionCommandPayload::Steer { .. }
                    | SessionCommandPayload::Queue { .. } => true,
                    SessionCommandPayload::Control { action, .. } => matches!(
                        action.as_ref(),
                        comet_doc::SessionControlAction::Start { .. }
                            | comet_doc::SessionControlAction::Steer { .. }
                            | comet_doc::SessionControlAction::Queue { .. }
                    ),
                    _ => false,
                };
                self.install_local_owner_grant(&p.command)?;
                let command_id = self
                    .doc_host
                    .queue_command(&p.chat_id, p.command)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                if activates_chat {
                    self.workspace
                        .set_chat_archived(&p.chat_id, false)
                        .map_err(|e| RpcError::Failed(e.to_string()))?;
                }
                RpcReply::value(&serde_json::json!({ "commandId": command_id }))
            }
            methods::TAKE_OVER_OMP_SESSION => {
                let p: ChatParams = parse_params(params)?;
                let run_id = self
                    .sessions
                    .take_over_omp_session(&p.chat_id)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({ "runId": run_id }))
            }
            methods::SEND_PEER_MESSAGE => {
                let p: SendPeerMessageParams = parse_params(params)?;
                if p.text.trim().is_empty() {
                    return Err(RpcError::Failed("empty_peer_message".into()));
                }
                let source_chat_id = canonical_session_id(&p.source_chat_id)
                    .ok_or_else(|| RpcError::Failed("invalid_source_chat_id".into()))?;
                let target_chat_id = canonical_session_id(&p.target_chat_id)
                    .ok_or_else(|| RpcError::Failed("invalid_target_chat_id".into()))?;
                if source_chat_id == target_chat_id {
                    return Err(RpcError::Failed("self_peer_message".into()));
                }
                let command_id = p.command_id.unwrap_or_else(crate::new_id);
                if command_id.trim().is_empty() {
                    return Err(RpcError::Failed("invalid_command_id".into()));
                }
                if p.wait && !self.doc_host.is_locally_hosted(&source_chat_id) {
                    return Err(RpcError::Failed("source_not_hosted".into()));
                }
                // Register before any target append: an immediately executing target
                // cannot race ahead of the local source/thread subscription.
                let registration = if p.wait {
                    Some(
                        self.sessions
                            .register_peer_waiter(&source_chat_id, &command_id)
                            .map_err(|e| RpcError::Failed(e.to_string()))?,
                    )
                } else {
                    None
                };
                // Membership MUST precede DocHost::open inside queue_command_with_id;
                // otherwise the missing-chat fallback could self-claim a foreign room.
                self.workspace
                    .upsert_session_ref(&target_chat_id, None)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let existing = self
                    .doc_host
                    .queue_command_with_id(
                        &target_chat_id,
                        &command_id,
                        SessionCommandPayload::PeerMessage {
                            text: p.text,
                            source_chat_id: source_chat_id.clone(),
                            thread_id: command_id.clone(),
                            reply_to: None,
                            hop_count: 0,
                        },
                    )
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let thread_id = match &existing.payload {
                    SessionCommandPayload::PeerMessage {
                        source_chat_id: stored_source_chat_id,
                        thread_id,
                        reply_to: None,
                        hop_count: 0,
                        ..
                    } if stored_source_chat_id == &source_chat_id && thread_id == &command_id => {
                        thread_id.clone()
                    }
                    _ => return Err(RpcError::Failed("command_id_conflict".into())),
                };
                let reply = match registration {
                    Some(registration) => self
                        .sessions
                        .wait_peer_reply(registration, peer_timeout(p.timeout_ms))
                        .await
                        .map(peer_reply_result),
                    None => None,
                };
                RpcReply::value(&PeerMessageResult {
                    command_id,
                    thread_id,
                    reply,
                })
            }
            methods::REPLY_PEER_MESSAGE => {
                let p: ReplyPeerMessageParams = parse_params(params)?;
                if p.text.trim().is_empty() {
                    return Err(RpcError::Failed("empty_peer_message".into()));
                }
                let session_id = canonical_session_id(&p.session_id)
                    .ok_or_else(|| RpcError::Failed("invalid_session_id".into()))?;
                if p.command_id.trim().is_empty() {
                    return Err(RpcError::Failed("invalid_command_id".into()));
                }
                let original = self
                    .doc_host
                    .command_entry(&session_id, &p.command_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("peer_command_not_found".into()))?;
                let (target_chat_id, thread_id, hop_count) = match original.payload {
                    SessionCommandPayload::PeerMessage {
                        source_chat_id,
                        thread_id,
                        hop_count,
                        ..
                    } => (source_chat_id, thread_id, hop_count),
                    _ => return Err(RpcError::Failed("not_peer_message".into())),
                };
                let target_chat_id = canonical_session_id(&target_chat_id)
                    .ok_or_else(|| RpcError::Failed("invalid_target_chat_id".into()))?;
                if thread_id.trim().is_empty() {
                    return Err(RpcError::Failed("invalid_thread_id".into()));
                }
                if hop_count >= 8 {
                    return Err(RpcError::Failed("peer_hop_limit".into()));
                }
                if target_chat_id == session_id {
                    return Err(RpcError::Failed("self_peer_message".into()));
                }
                if p.wait && !self.doc_host.is_locally_hosted(&session_id) {
                    return Err(RpcError::Failed("source_not_hosted".into()));
                }
                let registration = if p.wait {
                    Some(
                        self.sessions
                            .register_peer_waiter(&session_id, &thread_id)
                            .map_err(|e| RpcError::Failed(e.to_string()))?,
                    )
                } else {
                    None
                };
                self.workspace
                    .upsert_session_ref(&target_chat_id, None)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                // One reply is correlated to one stored command. Deriving its id
                // makes transport retries append/execute exactly once without
                // adding another durable idempotency store.
                let reply_command_id = format!("reply:{}", p.command_id);
                let source_session_id = session_id;
                let original_command_id = p.command_id.clone();
                let queued = self
                    .doc_host
                    .queue_command_with_id(
                        &target_chat_id,
                        &reply_command_id,
                        SessionCommandPayload::PeerMessage {
                            text: p.text,
                            source_chat_id: source_session_id.clone(),
                            thread_id: thread_id.clone(),
                            reply_to: Some(original_command_id.clone()),
                            hop_count: hop_count + 1,
                        },
                    )
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                match &queued.payload {
                    SessionCommandPayload::PeerMessage {
                        source_chat_id,
                        thread_id: queued_thread,
                        reply_to: Some(reply_to),
                        hop_count: queued_hop,
                        ..
                    } if source_chat_id == &source_session_id
                        && queued_thread == &thread_id
                        && reply_to == &original_command_id
                        && *queued_hop == hop_count + 1 => {}
                    _ => return Err(RpcError::Failed("command_id_conflict".into())),
                }
                let reply = match registration {
                    Some(registration) => self
                        .sessions
                        .wait_peer_reply(registration, peer_timeout(p.timeout_ms))
                        .await
                        .map(peer_reply_result),
                    None => None,
                };
                RpcReply::value(&PeerMessageResult {
                    command_id: reply_command_id,
                    thread_id,
                    reply,
                })
            }
            methods::WAIT_PEER_REPLY => {
                let p: WaitPeerReplyParams = parse_params(params)?;
                let source_chat_id = canonical_session_id(&p.source_chat_id)
                    .ok_or_else(|| RpcError::Failed("invalid_source_chat_id".into()))?;
                if p.thread_id.trim().is_empty() {
                    return Err(RpcError::Failed("invalid_thread_id".into()));
                }
                if !self.doc_host.is_locally_hosted(&source_chat_id) {
                    return Err(RpcError::Failed("source_not_hosted".into()));
                }
                let registration = self
                    .sessions
                    .register_peer_waiter(&source_chat_id, &p.thread_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let reply = self
                    .sessions
                    .wait_peer_reply(registration, peer_timeout(p.timeout_ms))
                    .await
                    .map(peer_reply_result);
                RpcReply::value(&PeerWaitResult {
                    thread_id: p.thread_id,
                    reply,
                })
            }
            methods::READ_DOC_MESSAGES => {
                let p: ReadDocMessagesParams = parse_params(params)?;
                let handle = self
                    .doc_host
                    .open_projection(&p.chat_id, p.room_projection.as_ref())
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let page = handle
                    .doc()
                    .read_entry_window(Some(p.before), comet_doc::TAIL_MESSAGE_COUNT)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&page)
            }
            methods::WATCH_DOC_MESSAGES => {
                let p: ChatParams = parse_params(params)?;
                let handle = self
                    .doc_host
                    .open_projection(&p.chat_id, p.room_projection.as_ref())
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(RpcReply::Stream(doc_messages_stream(
                    handle.watch_messages(),
                )))
            }
            "WatchCollaboration" => {
                let p: ChatParams = parse_params(params)?;
                let handle = self
                    .doc_host
                    .open_projection(&p.chat_id, p.room_projection.as_ref())
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let doc = handle.doc_arc();
                Ok(RpcReply::Stream(collaboration_stream(
                    handle.watch_messages(),
                    self.doc_host.watch_authority(),
                    doc,
                    self.doc_host.clone(),
                    self.auth.clone(),
                    p.room_projection,
                )))
            }
            methods::PROBE_SYNC => {
                self.workspace.probe();
                self.doc_host.probe_open_chats();
                RpcReply::value(&serde_json::json!({}))
            }
            methods::SYNC_STATUS => {
                fn room_json(s: &comet_sync::RoomStatsSnapshot) -> serde_json::Value {
                    serde_json::json!({
                        "connected": s.connected,
                        "lastPushedMs": s.last_pushed_ms,
                        "lastAckMs": s.last_ack_ms,
                        "rejoins": s.rejoins,
                        "probes": s.probes,
                        "fullResyncs": s.full_resyncs,
                        "disconnects": s.disconnects,
                    })
                }
                let workspace = self.workspace.sync_status();
                let chats: Vec<serde_json::Value> = self
                    .doc_host
                    .sync_statuses()
                    .iter()
                    .map(|(chat_id, room)| {
                        serde_json::json!({
                            "chatId": chat_id,
                            "room": room.as_ref().map(room_json),
                        })
                    })
                    .collect();
                RpcReply::value(&serde_json::json!({
                    "deviceId": self.doc_host.device_id(),
                    "nowMs": crate::now_ms(),
                    "workspace": workspace.as_ref().map(room_json),
                    "chats": chats,
                }))
            }
            methods::WATCH_CHATS => {
                Ok(RpcReply::Stream(watch_stream(self.workspace.watch_chats())))
            }
            methods::WATCH_DEVICES => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_devices(),
            ))),
            methods::WATCH_SPACES => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_spaces(),
            ))),
            methods::WATCH_SESSION_REFS => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_session_refs(),
            ))),
            methods::ADD_SESSION_REF => {
                let p: SessionRefParams = parse_params(params)?;
                let chat_id = canonical_session_id(&p.chat_id)
                    .ok_or_else(|| RpcError::Failed("invalid_session_id".into()))?;
                let session_ref = self
                    .workspace
                    .upsert_session_ref(&chat_id, None)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&session_ref)
            }
            methods::REMOVE_SESSION_REF => {
                let p: SessionRefParams = parse_params(params)?;
                let chat_id = canonical_session_id(&p.chat_id)
                    .ok_or_else(|| RpcError::Failed("invalid_session_id".into()))?;
                let removed = self
                    .workspace
                    .remove_session_ref(&chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&RemoveSessionRefResult { removed })
            }
            methods::WATCH_SCAFFOLD_ENVIRONMENTS => {
                Ok(RpcReply::Stream(watch_stream(self.scaffold()?.watch())))
            }
            methods::REFRESH_SCAFFOLD_ENVIRONMENTS => {
                let p: RefreshScaffoldEnvironmentsParams = parse_params(params)?;
                let cancellation = comet_harness::CancellationToken::new();
                let snapshot = self
                    .scaffold()?
                    .refresh(&p.scope, &cancellation)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::CONTROL_SCAFFOLD_ENVIRONMENT => {
                let control: ScaffoldEnvironmentControl = parse_params(params)?;
                let cancellation = comet_harness::CancellationToken::new();
                let scaffold = self.scaffold()?;
                let owner_room = self.prepare_scaffold_attach(&control)?;
                self.await_scaffold_owner_room(owner_room.as_deref(), &cancellation)
                    .await?;
                let result = scaffold
                    .control(control, &cancellation)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
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
                        .upsert_session_ref(
                            &projection.session_id,
                            Some(result.environment.clone()),
                        )
                        .map_err(|error| RpcError::Failed(error.to_string()))?;
                }
                if let Err(error) = self.install_scaffold_control_grant(&result) {
                    tracing::warn!(
                        error = %error,
                        "Scaffold attached without local control grant projection"
                    );
                }
                RpcReply::value(&result)
            }
            methods::WATCH_SESSIONS => {
                // Local live statuses merged with remote devices' workspace rows.
                let merged = self
                    .workspace
                    .merged_sessions_watch(self.sessions.watch_sessions());
                Ok(RpcReply::Stream(watch_stream(merged)))
            }
            methods::LOCAL_DEVICE => {
                RpcReply::value(&serde_json::json!({ "deviceId": self.doc_host.device_id() }))
            }
            methods::SCAFFOLD_HOST_AUTHORITY => {
                if self.runtime_profile != RuntimeProfile::ScaffoldHost {
                    return Err(RpcError::Failed(
                        "scaffold_host_authority_disabled_by_runtime_profile".into(),
                    ));
                }
                let authority = self.auth()?.device_grant_authority().ok_or_else(|| {
                    RpcError::Failed("scaffold_host_authority_unavailable".into())
                })?;
                RpcReply::value(&authority)
            }
            methods::UPDATE_STATUS => Ok(RpcReply::Stream(watch_stream(self.updater()?.watch()))),
            methods::STAGE_UPDATE => {
                let staged = self
                    .updater()?
                    .stage_mac_update()
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&serde_json::json!({ "ok": true, "path": staged }))
            }
            methods::APPLY_UPDATE => {
                let version = self
                    .updater()?
                    .apply()
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&serde_json::json!({ "ok": true, "version": version }))
            }
            methods::UPDATE_HARNESS => {
                let p: UpdateHarnessParams = parse_params(params)?;
                let status = self
                    .updater()?
                    .update_harness(&p.harness)
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&status)
            }
            methods::SET_HARNESS_AUTO_UPDATE => {
                let p: SetHarnessAutoUpdateParams = parse_params(params)?;
                self.updater()?.set_harness_auto_update(p.enabled);
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::MUTATE => {
                let p: MutateParams = parse_params(params)?;
                self.mutate(p)?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::WATCH_CHECKOUT_DIFFS => {
                Ok(RpcReply::Stream(watch_stream(self.diff_sync.watch_diffs())))
            }
            methods::READ_CHECKOUT_DIFF => {
                let p: ReadCheckoutDiffParams = parse_params(params)?;
                RpcReply::value(&ReadCheckoutDiffResult {
                    diff: self.diff_sync.read_diff(&p.checkout_id, &p.checksum),
                })
            }
            methods::LIST_REPOS => RpcReply::value(&self.repos.list().await),
            methods::ADD_REPO => {
                #[derive(Deserialize)]
                struct P {
                    path: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .add(&p.path)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::CLONE_REPO => {
                #[derive(Deserialize)]
                struct P {
                    url: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .clone_repo(&p.url)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::CREATE_REPO => {
                #[derive(Deserialize)]
                struct P {
                    name: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .create(&p.name)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::LIST_BRANCHES => {
                let p: RepoPathParams = parse_params(params)?;
                let branches = self
                    .repos
                    .branches(std::path::Path::new(&p.repo_path))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&branches)
            }
            methods::LIST_REFS => {
                let p: RepoPathParams = parse_params(params)?;
                let refs = self
                    .repos
                    .refs(std::path::Path::new(&p.repo_path))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&refs)
            }
            methods::SWITCH_REF => {
                let p: SwitchRefParams = parse_params(params)?;
                let branch = self
                    .repos
                    .switch_ref(std::path::Path::new(&p.repo_path), &p.ref_name)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "branch": branch }))
            }
            methods::LIST_FOLDERS => {
                let p: ListFoldersParams = parse_params(params)?;
                let listing = self
                    .repos
                    .list_folders(p.path)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&listing)
            }
            methods::SEARCH_FILES => {
                let p: FileSearchParams = parse_params(params)?;
                if p.query.chars().count() > 256 {
                    return Err(RpcError::BadParams(
                        "SearchFiles query must not exceed 256 characters".into(),
                    ));
                }
                let matches = tokio::time::timeout(FILE_SEARCH_RPC_TIMEOUT, async {
                    let root = self.file_search_root(&p).await?;
                    let featured_paths = p
                        .chat_id
                        .as_deref()
                        .filter(|_| p.query.is_empty())
                        .map(|chat_id| self.featured_file_paths(chat_id))
                        .unwrap_or_default();
                    self.repos
                        .search_files(root, p.query, featured_paths)
                        .await
                        .map_err(|e| RpcError::Failed(e.to_string()))
                })
                .await
                .map_err(|_| RpcError::Failed("file search timed out".into()))??;
                RpcReply::value(&matches)
            }
            methods::CREATE_WORKTREE => {
                let p: CreateWorktreeParams = parse_params(params)?;
                let worktree = self
                    .repos
                    .create_worktree(std::path::Path::new(&p.repo_path), &p.branch)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&worktree)
            }
            methods::DELETE_WORKTREE => {
                let p: DeleteWorktreeParams = parse_params(params)?;
                self.repos
                    .delete_worktree(
                        std::path::Path::new(&p.repo_path),
                        std::path::Path::new(&p.worktree_path),
                    )
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::OPEN_TERMINAL => {
                let p: OpenTerminalParams = parse_params(params)?;
                // The terminal runs in the chat's checkout; a chat with no cwd (or
                // no row yet) gets the home directory.
                let cwd = self
                    .workspace
                    .doc()
                    .chat(&p.chat_id)
                    .ok()
                    .flatten()
                    .and_then(|chat| chat.cwd)
                    .unwrap_or_else(|| home_dir().to_string_lossy().to_string());
                let session = self
                    .terminals
                    .open(&cwd, p.cols, p.rows)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&session)
            }
            methods::SUBSCRIBE_TERMINAL => {
                let p: SubscribeTerminalParams = parse_params(params)?;
                let rx = self
                    .terminals
                    .subscribe(&p.terminal_id, p.after_seq)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let stream = futures::stream::unfold(rx, |mut rx| async move {
                    let event = rx.recv().await?;
                    let value = serde_json::to_value(&event).ok()?;
                    Some((value, rx))
                });
                Ok(RpcReply::Stream(stream.boxed()))
            }
            methods::WRITE_TERMINAL => {
                let p: WriteTerminalParams = parse_params(params)?;
                self.terminals
                    .write(&p.terminal_id, &p.data)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::RESIZE_TERMINAL => {
                let p: ResizeTerminalParams = parse_params(params)?;
                self.terminals
                    .resize(&p.terminal_id, p.cols, p.rows)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::CLOSE_TERMINAL => {
                let p: TerminalIdParams = parse_params(params)?;
                self.terminals
                    .close(&p.terminal_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::GET_AGENT_ROUTE_RECEIPT => {
                let p: GetAgentRouteReceiptParams = parse_params(params)?;
                let cancellation = comet_harness::CancellationToken::new();
                let receipt = self
                    .scaffold()?
                    .client()
                    .get_agent_route_receipt(&p.logical_session_id, &cancellation)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&receipt)
            }
            methods::LIST_AGENT_ACCOUNTS => {
                self.require_agent_accounts()?;
                let snapshot = self
                    .agent_accounts
                    .list()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::MIGRATE_AGENT_ACCOUNT => {
                self.require_agent_accounts()?;
                let p: MigrateAgentAccountParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .migrate(p.harness, &p.account_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::REVOKE_AGENT_ACCOUNT => {
                self.require_agent_accounts()?;
                let p: RevokeAgentAccountParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .revoke(&p.account_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::START_AGENT_LOGIN => {
                self.require_agent_accounts()?;
                let p: StartAgentLoginParams = parse_params(params)?;
                let start = self
                    .agent_accounts
                    .start_login(p.harness)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&start)
            }
            methods::COMPLETE_AGENT_LOGIN => {
                self.require_agent_accounts()?;
                let p: CompleteAgentLoginParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .complete_login(&p.login_id, &p.code)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::POLL_AGENT_LOGIN => {
                self.require_agent_accounts()?;
                let p: LoginIdParams = parse_params(params)?;
                let poll = self
                    .agent_accounts
                    .poll_login(&p.login_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&poll)
            }
            methods::CANCEL_AGENT_LOGIN => {
                self.require_agent_accounts()?;
                let p: LoginIdParams = parse_params(params)?;
                self.agent_accounts.cancel_login(&p.login_id);
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::UPLOAD_CHUNK => {
                let p: UploadChunkParams = parse_params(params)?;
                self.uploads
                    .append(&p.upload_id, &p.data, p.seq)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::UPLOAD_COMMIT => {
                let p: UploadCommitParams = parse_params(params)?;
                let path = self
                    .uploads
                    .commit(&p.upload_id, &p.file_name)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "path": path }))
            }
            methods::READ_ATTACHMENT_CHUNK => {
                let p: ReadAttachmentChunkParams = parse_params(params)?;
                // Path jail: the uploads dir plus every workspace-known chat cwd.
                let roots: Vec<std::path::PathBuf> = self
                    .workspace
                    .doc()
                    .read_chats()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|chat| chat.cwd)
                    .map(std::path::PathBuf::from)
                    .collect();
                let chunk = self
                    .uploads
                    .read_chunk(&p.path, p.offset, &roots)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&chunk)
            }
            methods::TOOL_CALL_DETAIL => {
                let p: ToolCallDetailParams = parse_params(params)?;
                let sessions = self.sessions.clone();
                // Whole-journal replay is sync file I/O — off the reactor.
                let detail = tokio::task::spawn_blocking(move || {
                    sessions.tool_call_detail(&p.chat_id, &p.tool_id)
                })
                .await
                .map_err(|err| RpcError::Failed(format!("tool detail scan failed: {err}")))?
                .map_err(|err| RpcError::Failed(err.to_string()))?;
                match detail {
                    Some(d) => RpcReply::value(&serde_json::json!({
                        "found": true,
                        "input": d.input,
                        "output": d.output,
                        "isError": d.is_error,
                        "resolved": d.resolved,
                    })),
                    None => RpcReply::value(&serde_json::json!({ "found": false })),
                }
            }
            other => Err(RpcError::UnknownMethod(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_account_params_accept_global_shapes() {
        let migrate: MigrateAgentAccountParams = parse_params(serde_json::json!({
            "accountId": "acct-1",
            "harness": "claude-code",
        }))
        .expect("migrate account params");
        assert_eq!(migrate.account_id, "acct-1");
        assert_eq!(migrate.harness, HarnessId::ClaudeCode);

        let revoke: RevokeAgentAccountParams =
            parse_params(serde_json::json!({ "accountId": "acct-1" }))
                .expect("revoke account params");
        assert_eq!(revoke.account_id, "acct-1");
    }

    #[test]
    fn route_receipt_params_accept_only_logical_session_identity() {
        let receipt: GetAgentRouteReceiptParams = parse_params(serde_json::json!({
            "logicalSessionId": "session-1",
        }))
        .expect("route receipt params");
        assert_eq!(receipt.logical_session_id, "session-1");
        assert!(
            parse_params::<GetAgentRouteReceiptParams>(serde_json::json!({
                "logicalSessionId": "session-1",
                "ownerSubject": "caller-supplied-owner",
            }))
            .is_err()
        );
    }

    #[test]
    fn scaffold_host_server_denies_generic_harness_model_and_command_catalogs() {
        for method in [
            methods::LIST_HARNESSES,
            methods::LIST_MODELS,
            methods::LIST_HARNESS_COMMANDS,
        ] {
            assert!(!generic_catalog_allowed(
                RuntimeProfile::ScaffoldHost,
                method
            ));
            assert!(generic_catalog_allowed(
                RuntimeProfile::LocalController,
                method
            ));
            assert!(generic_catalog_allowed(RuntimeProfile::Mock, method));
        }
    }

    #[test]
    fn durable_commands_are_queued_locally_before_remote_delivery() {
        assert!(!forwardable(methods::LOCAL_DEVICE));
        assert!(!forwardable(methods::QUEUE_COMMAND));
        assert!(forwardable(methods::SEARCH_FILES));
        assert!(forwardable(methods::READ_CHECKOUT_DIFF));
        assert!(forwardable(methods::UPDATE_HARNESS));
        assert!(forwardable(methods::SET_HARNESS_AUTO_UPDATE));
        assert!(!forwardable(methods::SEND_PEER_MESSAGE));
        assert!(!forwardable(methods::REPLY_PEER_MESSAGE));
        assert!(!forwardable(methods::WAIT_PEER_REPLY));
    }

    #[test]
    fn peer_wait_timeout_is_capped_at_two_minutes() {
        assert_eq!(peer_timeout(None), Duration::from_millis(30_000));
        assert_eq!(peer_timeout(Some(1)), Duration::from_millis(1));
        assert_eq!(
            peer_timeout(Some(MAX_PEER_WAIT_MS + 1)),
            Duration::from_millis(MAX_PEER_WAIT_MS)
        );
    }

    #[test]
    fn only_the_worktree_owner_can_stage_shared_session_cleanup() {
        let mut chat = Chat {
            id: "chat-a".into(),
            device_id: "device-a".into(),
            title: None,
            archived: false,
            cwd: Some("/tmp/worktree".into()),
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::DateTime::UNIX_EPOCH,
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
        };
        assert!(owner_may_stage_worktree_deletion(
            "owner",
            "device-a",
            &chat,
            &[]
        ));
        chat.device_id = "device-b".into();
        assert!(!owner_may_stage_worktree_deletion(
            "owner",
            "device-a",
            &chat,
            &[]
        ));

        chat.device_id = "device-a".into();
        let sessions = vec![AgentSessionRecord {
            session_id: "session-a".into(),
            chat_id: chat.id.clone(),
            owner_subject: "owner".into(),
            owner_device_id: "device-a".into(),
            source: comet_proto::AgentSessionSource::Local,
            environment: None,
            harness: Some(HarnessId::Mock),
            model: None,
            harness_session_id: None,
            status: Some(SessionStatus::Idle),
            updated_at: Some(1),
            created_at: 1,
            unknown: Default::default(),
        }];
        assert!(owner_may_stage_worktree_deletion(
            "owner", "device-a", &chat, &sessions
        ));
        assert!(!owner_may_stage_worktree_deletion(
            "owner", "device-z", &chat, &sessions
        ));
        assert!(!owner_may_stage_worktree_deletion(
            "collaborator",
            "device-a",
            &chat,
            &sessions
        ));
    }

    #[test]
    fn staged_worktree_cleanup_waits_exactly_seven_days() {
        let now = chrono::DateTime::from_timestamp_millis(1_000).unwrap();
        assert_eq!(
            worktree_deletion_deadline(now).timestamp_millis(),
            1_000 + 7 * 24 * 60 * 60 * 1_000
        );
    }

    #[tokio::test]
    async fn collaboration_rpc_projection_reanchors_file_annotations_from_current_workspace_text() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        std::fs::create_dir_all(checkout.join("src")).unwrap();
        std::fs::write(checkout.join("src/lib.rs"), "prefix selected suffix").unwrap();
        let store = std::sync::Arc::new(comet_sync::DocsStore::open(dir.path()).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            crate::workspace_host::WorkspaceHostConfig {
                device_id: "device-a".into(),
                device_name: "test".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "accounts.google.com:subject-alice".into(),
                edge: None,
            },
        )
        .unwrap();
        workspace
            .claim_chat("chat-file", checkout.to_str())
            .unwrap();
        let workspace_id = workspace
            .doc()
            .chat("chat-file")
            .unwrap()
            .and_then(|chat| chat.space_id)
            .unwrap();
        let host = DocHost::new(
            store,
            crate::doc_host::DocHostConfig {
                device_id: "device-a".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        host.set_workspace(workspace);
        let doc = comet_doc::SessionDoc::init("chat-file").unwrap();
        let annotation = comet_proto::SemanticAnnotation {
            id: "annotation-file".into(),
            author_subject: "accounts.google.com:subject-alice".into(),
            body: "Review this".into(),
            anchor: comet_proto::SemanticAnchor {
                target_kind: comet_proto::AnchorTargetKind::File,
                target_id: format!("local-workspace:{workspace_id}:src/lib.rs"),
                file: Some(comet_proto::FileTargetReference::LocalWorkspacePath {
                    workspace_id: workspace_id.clone(),
                    relative_path: "src/lib.rs".into(),
                    unknown: Default::default(),
                }),
                byte_range: Some(comet_proto::Utf8ByteRange { start: 0, end: 8 }),
                exact: Some("selected".into()),
                prefix_hash: None,
                suffix_hash: None,
                unknown: Default::default(),
            },
            state: comet_proto::AnnotationState::Anchored,
            created_at: 11,
            resolved_at: None,
            unknown: Default::default(),
        };
        doc.append_publication(&comet_proto::PublicationRecord {
            id: "annotation/annotation-file/create/command-a".into(),
            schema_version: comet_proto::COLLABORATION_SCHEMA_VERSION,
            published_at: 11,
            published_by: annotation.author_subject.clone(),
            value: comet_proto::PublicationValue::Annotation(annotation),
            unknown: Default::default(),
        })
        .unwrap();
        let now = crate::now_ms();
        doc.append_publication(&comet_proto::PublicationRecord {
            id: "session/session-a/start".into(),
            schema_version: comet_proto::COLLABORATION_SCHEMA_VERSION,
            published_at: now,
            published_by: "accounts.google.com:subject-alice".into(),
            value: comet_proto::PublicationValue::AgentSession(Box::new(
                comet_proto::AgentSessionRecord {
                    session_id: "session-a".into(),
                    chat_id: "chat-file".into(),
                    owner_subject: "accounts.google.com:subject-alice".into(),
                    owner_device_id: "device-a".into(),
                    source: comet_proto::AgentSessionSource::Local,
                    environment: None,
                    harness: Some(HarnessId::Mock),
                    model: None,
                    harness_session_id: None,
                    status: Some(comet_proto::SessionStatus::Idle),
                    updated_at: Some(now),
                    created_at: now,
                    unknown: Default::default(),
                },
            )),
            unknown: Default::default(),
        })
        .unwrap();
        host.install_local_owner_grant(comet_proto::CapabilityGrant {
            id: "verified-local-grant".into(),
            principal_subject: "accounts.google.com:subject-alice".into(),
            scope: CollaborationScope {
                project_id: "project-a".into(),
                deployment_id: Some("project-a".into()),
                session_id: Some("session-a".into()),
                unknown: Default::default(),
            },
            capabilities: vec![comet_proto::CAPABILITY_SESSION_ANNOTATE.into()],
            sandbox_id: None,
            device_id: Some("device-a".into()),
            lifecycle_epoch: None,
            granted_by: "authenticated-local-identity".into(),
            granted_at: now,
            expires_at: Some(now + 60_000),
            revoked_at: None,
            unknown: Default::default(),
        })
        .unwrap();

        let auth = Auth::new(crate::auth::AuthConfig {
            edge_url: "http://127.0.0.1:8787".into(),
            data_dir: dir.path().join("auth"),
            scaffold_url: None,
            project_scope: "project-a".into(),
            oauth_scopes: String::new(),
            internal_capabilities: String::new(),
            dev_user_id: "accounts.google.com:subject-alice".into(),
            callback_port: None,
            device_join_grant: None,
            expected_device_id: None,
            expected_session_id: None,
            expected_deployment_id: None,
            expected_lifecycle_epoch: None,
            expected_sandbox_id: None,
        });
        let projection = project_collaboration_snapshot(&doc, &host, Some(&auth), None).unwrap();
        assert_eq!(
            projection
                .principal
                .as_ref()
                .map(|principal| principal.subject.as_str()),
            Some("accounts.google.com:subject-alice")
        );
        assert_eq!(projection.grants.len(), 1);
        assert_eq!(projection.grants[0].id, "verified-local-grant");
        host.install_scaffold_control_grant(comet_proto::CapabilityGrant {
            id: "attached-scaffold-grant".into(),
            principal_subject: "accounts.google.com:subject-alice".into(),
            scope: CollaborationScope {
                project_id: "project-a".into(),
                deployment_id: Some("deployment-a".into()),
                session_id: Some("chat-file".into()),
                unknown: Default::default(),
            },
            capabilities: vec![comet_proto::CAPABILITY_SESSION_CHAT.into()],
            sandbox_id: Some("sandbox-a".into()),
            device_id: Some("comet-scaffold-sandbox-a-e1".into()),
            lifecycle_epoch: Some(1),
            granted_by: "comet-edge-device-room".into(),
            granted_at: now,
            expires_at: Some(now + 60_000),
            revoked_at: None,
            unknown: Default::default(),
        })
        .unwrap();
        let room_projection = SessionRoomProjection {
            project_id: "project-a".into(),
            deployment_id: "deployment-a".into(),
            session_id: "chat-file".into(),
        };
        let attached_projection =
            project_collaboration_snapshot(&doc, &host, Some(&auth), Some(&room_projection))
                .unwrap();
        let attached_principal = attached_projection.principal.as_ref().unwrap();
        assert_eq!(
            attached_principal.deployment_id.as_deref(),
            Some("deployment-a")
        );
        assert_eq!(attached_principal.session_id.as_deref(), Some("chat-file"));
        assert_eq!(attached_projection.grants.len(), 1);
        assert_eq!(attached_projection.grants[0].id, "attached-scaffold-grant");
        let projected = projection
            .publications
            .iter()
            .find_map(|publication| match &publication.value {
                comet_proto::PublicationValue::Annotation(annotation) => Some(annotation),
                _ => None,
            })
            .unwrap();
        assert_eq!(projected.id, "annotation-file");
        assert_eq!(
            projected.anchor.byte_range,
            Some(comet_proto::Utf8ByteRange { start: 7, end: 15 })
        );
        assert_eq!(projected.state, comet_proto::AnnotationState::Reanchored);
    }

    #[tokio::test]
    async fn scaffold_attach_preparation_does_not_wait_for_the_remote_owner() {
        let dir = tempfile::tempdir().unwrap();
        let core = crate::EngineCore::assemble_with_identity(
            dir.path(),
            std::sync::Arc::new(crate::default_registry(RuntimeProfile::Mock)),
            HarnessId::Mock,
            None,
            "project-a",
            "accounts.google.com:subject-alice",
            RuntimeProfile::Mock,
        )
        .unwrap();
        let rpc = core.rpc_service();
        let control = ScaffoldEnvironmentControl::Attach {
            sandbox_id: "sandbox-a".into(),
            scope: CollaborationScope {
                project_id: "project-a".into(),
                deployment_id: Some("deployment-a".into()),
                session_id: Some("session-a".into()),
                unknown: Default::default(),
            },
        };

        tokio::time::timeout(Duration::from_millis(100), async {
            rpc.prepare_scaffold_attach(&control)
        })
        .await
        .expect("preparing an attach must not wait for the stopped remote owner")
        .unwrap();
        core.shutdown().await;
    }

    #[test]
    fn tool_file_paths_keep_workspace_activity_only() {
        assert_eq!(
            tool_file_path(&ToolCall::EditFile {
                path: "src/main.rs".into(),
                old_string: None,
                new_string: None,
            }),
            Some("src/main.rs")
        );
        assert_eq!(
            tool_file_path(&ToolCall::Exec {
                command: "cargo test".into(),
            }),
            None
        );
    }
}
