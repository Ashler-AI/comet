//! DocHost — shared-thread `SessionDoc` handles: snapshot persistence, edge sync, and
//! per-agent-session durable command execution.
//!
//! The doc is the outbox: immutable publications and commands commit locally and sync
//! whenever a room connection exists. Each agent session has its own execution key and
//! owning device/principal, so multiple teammates can run concurrently in one thread.
//! A device drains only commands targeting sessions it owns, checks the verified scoped
//! grant, marks processed before the side effect, and appends an actor-attributed audit.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use sha2::{Digest, Sha256};
use tokio::sync::watch;

use comet_doc::{
    COMMAND_DEFAULT_TTL_MS, CommandBasedOn, CommandDisposition, DocError, EvaluationContext,
    MessagePart, MessageRole, MessageStatus, SessionCommandEntry, SessionCommandPayload,
    SessionCommandStatus, SessionControlAction, SessionDoc, SessionEntryWindow,
    SessionMessageEntry, TAIL_MESSAGE_COUNT, evaluate_command,
};
use comet_proto::{
    AgentSessionRecord, AuditEvent, AuditResult, COLLABORATION_SCHEMA_VERSION, CapabilityGrant,
    FileTargetReference, HarnessId, MessageProvenance, ModelHandoff, PublicationRecord,
    PublicationValue, SemanticAnchor, SemanticAnnotation, SessionRoomProjection, SessionStatus,
    UserInputAnswer, UserInputQuestion, VerifiedCapabilityGrantEnvelope,
};
use comet_sync::{DocsStore, RoomClient};

use crate::sessions::{PeerReply, QueueOutcome, SessionsEngine, SteerOutcome};
use crate::workspace_host::WorkspaceHost;
use crate::{EngineError, new_id, now_ms};

/// Debounce window for local snapshot saves after a doc change.
const SNAPSHOT_DEBOUNCE_MS: u64 = 1_000;

/// Authenticated local-owner grants are intentionally short-lived. A queued
/// command may survive a process restart, so its durable authority proof uses
/// the same bounded lifetime when the in-memory grant is no longer present.
pub(crate) const LOCAL_OWNER_GRANT_TTL_MS: i64 = 5 * 60 * 1_000;

const LOCAL_OWNER_AUTHORITY_KEY_PREFIX: &str = "local-control-authority/v1/";
const EDGE_GRANT_CAPABILITIES: &[&str] = &[
    comet_proto::CAPABILITY_SESSION_READ,
    comet_proto::CAPABILITY_SESSION_CHAT,
    comet_proto::CAPABILITY_SESSION_CONTROL,
    comet_proto::CAPABILITY_SESSION_ANNOTATE,
    "session.invite",
    comet_proto::CAPABILITY_SESSION_FILES,
    comet_proto::CAPABILITY_SESSION_ENVIRONMENT,
];

/// Warm-doc LRU: how many unwatched, run-less docs stay fully open. Everything
/// beyond this (and beyond [`comet_doc::DOC_LRU_BYTE_BUDGET`]) is evicted
/// oldest-access-first — reopening from the SQLite snapshot measured within
/// ~11ms of a warm doc, so the cap trades no perceptible open latency.
const WARM_DOC_CAP: usize = 12;

/// Resident-memory estimate per compressed snapshot byte. Loro snapshots are
/// columnar+compressed; the in-memory doc plus mirror runs well above the blob
/// size. A rough multiplier is enough here — the budget is a safety ceiling,
/// the count cap does the day-to-day work.
const RESIDENT_BYTES_PER_SNAPSHOT_BYTE: usize = 6;

/// Floor per open doc (room socket buffers, tasks) regardless of content size.
const DOC_RESIDENT_FLOOR_BYTES: usize = 512 * 1024;

/// Docs touched this recently are never evicted. Closes the open→attach race:
/// `open()` returns a handle, and until the caller's `watch_messages` lands
/// the doc is unwatched and unpinned — a concurrent eviction would orphan the
/// watcher on a roomless doc that renders once and never updates again.
const EVICT_MIN_IDLE_MS: i64 = 30_000;

/// Edge connection config. The bearer is a provider, never a snapshot: every
/// reconnect and HTTP request re-reads it so a revoked or replaced credential
/// does not remain captured in room clients.
#[derive(Clone)]
pub struct EdgeConfig {
    /// Edge base URL (`http(s)://…`); rewritten to `ws(s)` for the room socket.
    pub url: String,
    /// Fresh-bearer provider (the relay's `TokenSource`), consulted per
    /// connect/request. `None` from the provider = signed out.
    pub token: Arc<dyn comet_rpc::TokenSource>,
    /// This engine's device id, carried on room dials (`&device=`) so the
    /// edge can attribute sockets in logs. Debugging the 2026-08-04 deaf
    /// socket meant reverse-engineering devices from rotating IPv6 privacy
    /// addresses; never again. Empty = omitted (tests).
    pub device_id: String,
    /// Deployment namespace for a scoped sandbox session room. Empty keeps
    /// legacy local-controller rooms on the project/session namespace.
    pub deployment_id: String,
}

impl std::fmt::Debug for EdgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeConfig")
            .field("url", &self.url)
            .field("token", &"<provider>")
            .field("deployment_id", &self.deployment_id)
            .finish()
    }
}

impl EdgeConfig {
    pub fn new(url: impl Into<String>, token: Arc<dyn comet_rpc::TokenSource>) -> Self {
        Self {
            url: url.into(),
            token,
            device_id: String::new(),
            deployment_id: String::new(),
        }
    }

    /// Attribute this engine's room sockets in edge logs.
    pub fn with_device(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = device_id.into();
        self
    }

    /// Select the deployment-scoped physical SessionRoom namespace.
    pub fn with_deployment(mut self, deployment_id: impl Into<String>) -> Self {
        self.deployment_id = deployment_id.into();
        self
    }

    /// Fixed bearer — dev mode and tests, where tokens never expire.
    pub fn with_static_token(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::new(url, Arc::new(comet_rpc::StaticToken(token.into())))
    }

    /// The current bearer, refreshed by the provider if stale. `None` = signed out.
    pub async fn bearer(&self) -> Option<String> {
        self.token.token().await
    }

    /// A per-dial room URL provider for `path` (e.g. `/session/{chatId}/ws`):
    /// the bearer is re-fetched before every connect, so reconnects after a
    /// token expiry present a fresh `?token=` instead of the boot-time one.
    pub fn room_url(&self, path: impl Into<String>) -> Arc<dyn comet_sync::UrlProvider> {
        self.room_url_for(path, None)
    }

    fn room_url_for(
        &self,
        path: impl Into<String>,
        projection: Option<&SessionRoomProjection>,
    ) -> Arc<dyn comet_sync::UrlProvider> {
        let ws_base = self.url.replacen("http", "ws", 1);
        Arc::new(EdgeRoomUrl {
            base: format!("{}{}", ws_base.trim_end_matches('/'), path.into()),
            token: self.token.clone(),
            device_id: self.device_id.clone(),
            deployment_id: projection
                .map(|scope| scope.deployment_id.clone())
                .unwrap_or_else(|| self.deployment_id.clone()),
        })
    }
}

struct EdgeRoomUrl {
    base: String,
    token: Arc<dyn comet_rpc::TokenSource>,
    device_id: String,
    deployment_id: String,
}

impl comet_sync::UrlProvider for EdgeRoomUrl {
    fn url(&self) -> futures::future::BoxFuture<'static, Result<String, comet_sync::SyncError>> {
        let token = self.token.clone();
        let base = self.base.clone();
        let device = self.device_id.clone();
        let deployment = self.deployment_id.clone();
        Box::pin(async move {
            let token = token.token().await.ok_or_else(|| {
                comet_sync::SyncError::Auth("no access token (signed out)".into())
            })?;
            let mut url = format!("{base}?token={token}");
            if !device.is_empty() {
                url.push_str(&format!("&device={device}"));
            }
            if !deployment.is_empty() {
                url.push_str(&format!("&deploymentId={deployment}"));
            }
            Ok(url)
        })
    }
}

#[derive(Debug, Clone)]
pub struct DocHostConfig {
    pub device_id: String,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// When present, each opened chat joins its edge session room. `None` = fully
    /// offline operation (local snapshots only).
    pub edge: Option<EdgeConfig>,
}

#[derive(Clone)]
struct TrustedGrant {
    grant: CapabilityGrant,
    edge_derived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandNudgeRoute {
    None,
    WorkspaceHost,
    ExactDevice(String),
}
struct DocHostInner {
    store: Arc<DocsStore>,
    config: DocHostConfig,
    sessions: OnceLock<SessionsEngine>,
    workspace: OnceLock<WorkspaceHost>,
    handles: Mutex<HashMap<String, Arc<ChatDocHandle>>>,
    /// Host-local authority populated only by authenticated relay frames or the
    /// authenticated local identity path. Never imported from Loro.
    trusted_grants: Mutex<HashMap<String, TrustedGrant>>,
    /// False between relay reconnect and the first verified grant frame.
    /// Remote commands remain pending while authority is being refreshed.
    edge_grants_ready: AtomicBool,
    /// Invalidates collaboration projections when host-local grants change.
    authority_tx: watch::Sender<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Redial cadence ceiling once the edge has ANSWERED with an HTTP rejection
/// (403 foreign scope, 404 unroutable room): the answer stays identical until
/// something about the session or credential changes, so hammering at the
/// 30s transport cap only spams the Worker and the logs (2026-08 field logs:
/// 1.3k warns from three such chats). Mirrors the workspace room's 15-minute
/// probe cadence; a system wake still resets to the fast base. 401 stays on
/// the fast cap — the next dial re-reads the bearer, so an expired token
/// heals within one refresh.
const REJECTED_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Backoff ceiling for one failed session-room dial: policy rejections crawl,
/// transport faults keep the fast reconnect cap.
fn join_retry_cap(err: &comet_sync::SyncError) -> std::time::Duration {
    match err {
        comet_sync::SyncError::HttpRejected(403 | 404) => REJECTED_RETRY_CAP,
        _ => crate::workspace_host::JOIN_RETRY_CAP,
    }
}

/// Bind durable local authority to the complete immutable portion of a command.
/// The SQLite row is written only after an authenticated local grant authorizes
/// this exact entry, so a synced peer cannot reuse an id with altered payload.
fn local_owner_authority_key(entry: &SessionCommandEntry) -> Result<String, serde_json::Error> {
    let immutable = (
        entry.id.as_str(),
        &entry.payload,
        entry.issued_by.as_str(),
        entry.issued_at,
        &entry.based_on,
        entry.expires_at,
    );
    let digest = Sha256::digest(serde_json::to_vec(&immutable)?);
    Ok(format!(
        "{LOCAL_OWNER_AUTHORITY_KEY_PREFIX}{}",
        crate::repos::hex(&digest)
    ))
}

fn grant_authorizes_control_scope(
    grant: &CapabilityGrant,
    entry: &SessionCommandEntry,
    project_scope: &str,
    now: i64,
) -> bool {
    let SessionCommandPayload::Control {
        actor_device_id,
        actor_subject,
        owner_device_id,
        grant_id,
        action,
        session_id,
        ..
    } = &entry.payload
    else {
        return false;
    };
    !actor_device_id.trim().is_empty()
        && !actor_subject.trim().is_empty()
        && grant.id == *grant_id
        && grant.scope.project_id == project_scope
        && grant
            .scope
            .deployment_id
            .as_deref()
            .is_some_and(|deployment| !deployment.is_empty())
        && grant.scope.session_id.as_deref() == Some(session_id.as_str())
        && grant.device_id.as_deref() == Some(owner_device_id.as_str())
        && grant.granted_at <= now
        && grant.expires_at.is_some_and(|expires| now < expires)
        && grant.revoked_at.is_none()
        && grant
            .capabilities
            .iter()
            .any(|capability| capability == action.required_capability())
}

fn grant_authorizes_control_entry(
    grant: &CapabilityGrant,
    entry: &SessionCommandEntry,
    project_scope: &str,
    now: i64,
) -> bool {
    let SessionCommandPayload::Control { actor_subject, .. } = &entry.payload else {
        return false;
    };
    grant.principal_subject == *actor_subject
        && grant_authorizes_control_scope(grant, entry, project_scope, now)
}

fn grant_matches_projected_scaffold_room(
    grant: &CapabilityGrant,
    chat_id: &str,
    projection: Option<&SessionRoomProjection>,
) -> bool {
    let Some(projection) = projection else {
        return false;
    };
    grant.scope.session_id.as_deref() == Some(chat_id)
        && projection.session_id == chat_id
        && grant.scope.project_id == projection.project_id
        && grant.scope.deployment_id.as_deref() == Some(projection.deployment_id.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnnotationMutationError {
    NotFound,
    NotAuthor,
}

/// Select an annotation mutation target only by its stable annotation id, then bind it to the
/// exact authenticated subject. Anchor target ids and display names are deliberately irrelevant.
fn annotation_revision_for_subject(
    publications: &[PublicationRecord],
    annotation_id: &str,
    actor_subject: &str,
) -> Result<SemanticAnnotation, AnnotationMutationError> {
    let annotation = publications
        .iter()
        .rev()
        .find_map(|publication| match &publication.value {
            PublicationValue::Annotation(annotation) if annotation.id == annotation_id => {
                Some(annotation)
            }
            _ => None,
        })
        .ok_or(AnnotationMutationError::NotFound)?;
    if annotation.author_subject != actor_subject {
        return Err(AnnotationMutationError::NotAuthor);
    }
    Ok(annotation.clone())
}

#[derive(Clone)]
pub struct DocHost {
    inner: Arc<DocHostInner>,
}

/// One open chat doc: the `SessionDoc`, its change plumbing, and the room client.
pub struct ChatDocHandle {
    chat_id: String,
    device_id: String,
    doc: Arc<SessionDoc>,
    messages_tx: watch::Sender<SessionEntryWindow>,
    /// True when the doc changed while nobody watched: the bounded tail mirror
    /// is rebuilt on the next `watch_messages` attach.
    mirror_dirty: AtomicBool,
    /// Epoch ms of the last open/watch touch — the LRU eviction key.
    last_access: AtomicI64,
    /// Last known snapshot blob size — the eviction budget estimate's input.
    snapshot_bytes: AtomicUsize,
    room_projection: Mutex<Option<SessionRoomProjection>>,
    room: Mutex<Option<RoomClient>>,
    /// Invalidates a room join that was already dialing when an unprojected
    /// local handle adopts its trusted Scaffold projection.
    room_generation: AtomicU64,
    /// Serializes idempotent command-id checks with appends for this doc.
    command_lock: Mutex<()>,
    room_join_started: AtomicBool,
    /// Doc subscription (drop = unsubscribe) — bumps the change watch on every commit.
    _sub: loro::Subscription,
}

impl ChatDocHandle {
    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    pub fn doc(&self) -> &SessionDoc {
        &self.doc
    }

    pub fn doc_arc(&self) -> Arc<SessionDoc> {
        self.doc.clone()
    }

    /// Bounded joined transcript tail watch (`WatchDocMessages`).
    ///
    /// Attach-time refresh: the mirror is only maintained while watched, so a
    /// doc that changed unwatched materializes here, once, instead of on every
    /// commit it sat through in the background.
    pub fn watch_messages(&self) -> watch::Receiver<SessionEntryWindow> {
        self.touch();
        // Attach is a user signal: verify a quiet room is actually alive
        // (a doc-wedged DO keeps answering pings while delivering nothing,
        // and the background probe cadence can be hours out). Coalescing
        // no-op on a healthy or recently-active room.
        if let Some(room) = lock(&self.room).as_ref() {
            room.probe();
        }
        // Subscribe BEFORE the dirty check: a commit racing this attach then
        // sees a live receiver and publishes, instead of re-marking dirty
        // after our refresh and leaving the new watcher a cleared mirror.
        let rx = self.messages_tx.subscribe();
        if self.mirror_dirty.load(Ordering::Acquire) {
            self.publish_messages();
        }
        rx
    }

    fn touch(&self) {
        self.last_access.store(now_ms(), Ordering::Relaxed);
    }

    /// Adopt the exact trusted Scaffold room for a handle that was opened
    /// locally before attach completed. A handle may move from unprojected to
    /// projected once. Projection-aware opens must keep matching exactly;
    /// ordinary opens preserve an established trusted projection.
    fn ensure_room_projection(
        &self,
        requested: Option<&SessionRoomProjection>,
    ) -> Result<(), EngineError> {
        let mut current = lock(&self.room_projection);
        match (current.as_ref(), requested) {
            (_, None) => return Ok(()),
            (Some(existing), Some(requested)) if existing == requested => return Ok(()),
            (Some(_), Some(_)) => {
                return Err(EngineError::Other(
                    "chat is already open with a different session room projection".into(),
                ));
            }
            (None, Some(requested)) => {
                *current = Some(requested.clone());
            }
        }
        drop(current);

        // A join may already be connected or dialing the legacy room. Bump the
        // generation before dropping the current client so an old dial cannot
        // install itself after the new projected join starts.
        self.room_generation.fetch_add(1, Ordering::AcqRel);
        drop(lock(&self.room).take());
        self.room_join_started.store(false, Ordering::Release);
        Ok(())
    }

    pub fn connected(&self) -> bool {
        lock(&self.room).is_some()
    }

    /// Write a complete user message entry, idempotent by id (the client-minted message
    /// id — a re-executed command or optimistic echo never duplicates the entry).
    pub fn write_user_message(
        &self,
        message_id: &str,
        text: &str,
        created_at: i64,
    ) -> Result<(), DocError> {
        self.write_user_message_with_status(message_id, text, created_at, MessageStatus::Complete)
    }

    /// Write a user message with its delivery state. Queue and steer paths use
    /// this so transcript feedback follows the durable message across devices.
    pub fn write_user_message_with_status(
        &self,
        message_id: &str,
        text: &str,
        created_at: i64,
        status: MessageStatus,
    ) -> Result<(), DocError> {
        if self.doc.read_entries()?.iter().any(|e| e.id == message_id) {
            return Ok(());
        }
        self.doc.push_message(&SessionMessageEntry {
            id: message_id.to_string(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.to_string(),
            }],
            created_at,
            device_id: self.device_id.clone(),
            status: Some(status),
            continuation_of: None,
        })
    }

    /// Recovery sweep: stamp this device's abandoned `streaming` entries `aborted`, appending
    /// `note` as a visible error part so the transcript says WHY the turn
    /// ended (comet folded "Run interrupted by backend restart" the same
    /// way). Returns the stamped entries' `(id, created_at)` — recovery uses
    /// them for the resume-freshness check.
    pub fn mark_abandoned_streams(&self, note: &str) -> Result<Vec<(String, i64)>, DocError> {
        let mut stamped = Vec::new();
        for entry in self.doc.read_entries()? {
            if entry.role == MessageRole::Assistant
                && entry.status == Some(MessageStatus::Streaming)
                && entry.device_id == self.device_id
                && self
                    .doc
                    .set_message_status(&entry.id, MessageStatus::Aborted)?
            {
                let part_id = format!("{}-recovery", entry.id);
                if let Err(err) = self.doc.append_error_part(&entry.id, &part_id, note) {
                    tracing::warn!(chat = %self.chat_id, error = %err, "recovery note append failed");
                }
                stamped.push((entry.id.clone(), entry.created_at));
            }
        }
        if !stamped.is_empty() {
            self.publish_messages();
        }
        Ok(stamped)
    }

    fn publish_messages(&self) {
        self.mirror_dirty.store(false, Ordering::Release);
        match self.doc.read_entry_window(None, TAIL_MESSAGE_COUNT) {
            Ok(window) => {
                // send_replace: update the watch even with no subscribers yet,
                // so a late subscriber's first borrow sees the current tail.
                self.messages_tx.send_replace(window);
            }
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err, "transcript read failed");
            }
        }
    }

    /// Per-commit publish path: unwatched docs just mark the mirror dirty —
    /// rebuilding even a bounded tail nobody reads is wasted work.
    fn publish_messages_if_watched(&self) {
        if self.messages_tx.receiver_count() == 0 {
            self.mirror_dirty.store(true, Ordering::Release);
        } else {
            self.publish_messages();
        }
    }

    /// Rough resident cost for the LRU budget.
    fn resident_estimate(&self) -> usize {
        (self.snapshot_bytes.load(Ordering::Relaxed) * RESIDENT_BYTES_PER_SNAPSHOT_BYTE)
            .max(DOC_RESIDENT_FLOOR_BYTES)
    }
}

fn append_agent_session_status(
    handle: &ChatDocHandle,
    session_id: &str,
    status: comet_proto::SessionStatus,
    published_by: &str,
    publication_id: String,
) -> Result<(), EngineError> {
    let Some(mut session) = handle
        .doc
        .collaboration_snapshot()?
        .sessions
        .into_iter()
        .find(|session| session.session_id == session_id)
    else {
        return Ok(());
    };
    let at = now_ms();
    session.status = Some(status);
    session.updated_at = Some(at);
    handle.doc.append_publication(&PublicationRecord {
        id: publication_id,
        schema_version: COLLABORATION_SCHEMA_VERSION,
        published_at: at,
        published_by: published_by.to_string(),
        value: PublicationValue::AgentSession(Box::new(session)),
        unknown: Default::default(),
    })?;
    Ok(())
}

fn monitor_agent_session_terminal(
    sessions: &SessionsEngine,
    handle: Arc<ChatDocHandle>,
    execution_key: String,
    session_id: String,
    published_by: String,
    command_id: String,
) {
    let mut statuses = sessions.watch_sessions();
    tokio::spawn(async move {
        loop {
            let status = statuses
                .borrow()
                .iter()
                .find(|session| session.chat_id == execution_key)
                .map(|session| session.status);
            if let Some(status @ (SessionStatus::Idle | SessionStatus::Errored)) = status {
                if let Err(err) = append_agent_session_status(
                    &handle,
                    &session_id,
                    status,
                    &published_by,
                    format!("session/{session_id}/terminal/{command_id}"),
                ) {
                    tracing::warn!(
                        chat = %handle.chat_id,
                        session = %session_id,
                        error = %err,
                        "terminal session state publication failed"
                    );
                }
                return;
            }
            if statuses.changed().await.is_err() {
                return;
            }
        }
    });
}

impl DocHost {
    pub fn new(store: Arc<DocsStore>, config: DocHostConfig) -> Self {
        let (authority_tx, _) = watch::channel(0);
        Self {
            inner: Arc::new(DocHostInner {
                store,
                config,
                sessions: OnceLock::new(),
                workspace: OnceLock::new(),
                handles: Mutex::new(HashMap::new()),
                trusted_grants: Mutex::new(HashMap::new()),
                edge_grants_ready: AtomicBool::new(false),
                authority_tx,
            }),
        }
    }

    /// Wire the sessions engine (engine assembly; see `SessionsEngine::set_doc_host`).
    pub fn set_sessions(&self, sessions: SessionsEngine) {
        let _ = self.inner.sessions.set(sessions);
        // Commands may already be pending in warm-opened docs.
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            let host = self.clone();
            tokio::spawn(async move { host.drain_commands(&handle).await });
        }
    }

    /// Wire the workspace host (engine assembly) — the source of chat-ownership rows.
    pub fn set_workspace(&self, workspace: WorkspaceHost) {
        let _ = self.inner.workspace.set(workspace);
    }

    /// The workspace host, once wired (tests may assemble a DocHost without one).
    pub fn workspace(&self) -> Option<&WorkspaceHost> {
        self.inner.workspace.get()
    }

    fn expire_grant_at(&self, grant_id: String, expires_at: i64) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let inner = Arc::downgrade(&self.inner);
        runtime.spawn(async move {
            let delay_ms = expires_at.saturating_sub(now_ms()).max(0) as u64;
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            let Some(inner) = inner.upgrade() else {
                return;
            };
            let removed = {
                let mut grants = lock(&inner.trusted_grants);
                let should_remove = grants.get(&grant_id).is_some_and(|trusted| {
                    trusted.grant.expires_at == Some(expires_at) && now_ms() >= expires_at
                });
                if should_remove {
                    grants.remove(&grant_id)
                } else {
                    None
                }
            };
            if removed.is_some() {
                inner
                    .authority_tx
                    .send_modify(|version| *version = version.wrapping_add(1));
            }
        });
    }

    pub(crate) fn ingest_verified_grant(
        &self,
        stream_session_id: &str,
        payload: &[u8],
    ) -> Result<(), &'static str> {
        let allowed_capabilities = EDGE_GRANT_CAPABILITIES;
        let envelope: VerifiedCapabilityGrantEnvelope =
            serde_json::from_slice(payload).map_err(|_| "grant_envelope_invalid")?;
        let workspace = self.workspace().ok_or("workspace_unavailable")?;
        let expected_device_id = self.device_id();
        let expected_identity = comet_proto::parse_scaffold_device_id(expected_device_id);
        let now = now_ms();
        let grant = &envelope.grant;
        let deployment_id = grant.scope.deployment_id.as_deref().unwrap_or_default();
        let expected_room_id = format!(
            "s4/{}/{}/{}",
            workspace.project_scope(),
            deployment_id,
            stream_session_id,
        );
        if envelope.room_id != expected_room_id
            || envelope.target_device_id != expected_device_id
            || envelope.target_session_id != stream_session_id
            || grant.id.trim().is_empty()
            || grant.principal_subject.trim().is_empty()
            || grant.scope.project_id != workspace.project_scope()
            || grant
                .scope
                .deployment_id
                .as_deref()
                .is_none_or(str::is_empty)
            || grant.scope.session_id.as_deref() != Some(stream_session_id)
            || grant.device_id.as_deref() != Some(expected_device_id)
            || expected_identity.is_none_or(|(sandbox, lifecycle_epoch)| {
                grant.sandbox_id.as_deref() != Some(sandbox)
                    || grant.lifecycle_epoch != Some(lifecycle_epoch)
            })
            || grant.capabilities.is_empty()
            || grant
                .capabilities
                .iter()
                .any(|capability| !allowed_capabilities.contains(&capability.as_str()))
            || grant.granted_at > now
            || !grant.expires_at.is_some_and(|expires| now < expires)
            || grant.revoked_at.is_some()
        {
            return Err("grant_envelope_scope_rejected");
        }
        let projection = SessionRoomProjection {
            project_id: grant.scope.project_id.clone(),
            deployment_id: deployment_id.to_owned(),
            session_id: stream_session_id.to_owned(),
        };
        self.open_projection(stream_session_id, Some(&projection))
            .map_err(|_| "grant_room_projection_rejected")?;
        {
            let mut grants = lock(&self.inner.trusted_grants);
            grants.retain(|_, trusted| !trusted.edge_derived);
            grants.insert(
                grant.id.clone(),
                TrustedGrant {
                    grant: grant.clone(),
                    edge_derived: true,
                },
            );
            self.inner.edge_grants_ready.store(true, Ordering::Release);
        }
        self.inner
            .authority_tx
            .send_modify(|version| *version = version.wrapping_add(1));
        self.expire_grant_at(
            grant.id.clone(),
            grant.expires_at.expect("validated expiry"),
        );
        Ok(())
    }

    /// Install the non-secret half of a control-plane device grant on the
    /// attaching viewport. Execution authority remains on the target device;
    /// this copy is only a trusted route selector for commands sent there.
    pub(crate) fn install_scaffold_control_grant(
        &self,
        grant: CapabilityGrant,
    ) -> Result<(), &'static str> {
        let workspace = self.workspace().ok_or("workspace_unavailable")?;
        let now = now_ms();
        let device_id = grant.device_id.as_deref().unwrap_or_default();
        let expected_identity = comet_proto::parse_scaffold_device_id(device_id);
        if grant.id.trim().is_empty()
            || grant.principal_subject.trim().is_empty()
            || grant.granted_by != "comet-edge-device-room"
            || grant.scope.project_id != workspace.project_scope()
            || grant
                .scope
                .deployment_id
                .as_deref()
                .is_none_or(str::is_empty)
            || grant.scope.session_id.as_deref().is_none_or(str::is_empty)
            || expected_identity.is_none_or(|(sandbox, lifecycle_epoch)| {
                grant.sandbox_id.as_deref() != Some(sandbox)
                    || grant.lifecycle_epoch != Some(lifecycle_epoch)
            })
            || grant.capabilities.is_empty()
            || grant
                .capabilities
                .iter()
                .any(|capability| !EDGE_GRANT_CAPABILITIES.contains(&capability.as_str()))
            || grant.granted_at > now
            || !grant.expires_at.is_some_and(|expires| now < expires)
            || grant.revoked_at.is_some()
        {
            return Err("scaffold_control_grant_scope_rejected");
        }
        let grant_id = grant.id.clone();
        let expires_at = grant.expires_at.expect("validated expiry");
        lock(&self.inner.trusted_grants).insert(
            grant.id.clone(),
            TrustedGrant {
                grant,
                edge_derived: false,
            },
        );
        self.inner
            .authority_tx
            .send_modify(|version| *version = version.wrapping_add(1));
        self.expire_grant_at(grant_id, expires_at);
        Ok(())
    }

    pub(crate) fn install_local_owner_grant(
        &self,
        grant: CapabilityGrant,
    ) -> Result<(), &'static str> {
        let workspace = self.workspace().ok_or("workspace_unavailable")?;
        let now = now_ms();
        if grant.granted_by != "authenticated-local-identity"
            || grant.scope.project_id != workspace.project_scope()
            || grant
                .scope
                .deployment_id
                .as_deref()
                .is_none_or(str::is_empty)
            || grant.scope.session_id.as_deref().is_none_or(str::is_empty)
            || grant.device_id.as_deref() != Some(self.device_id())
            || grant.granted_at > now
            || !grant.expires_at.is_some_and(|expires| now < expires)
            || grant.revoked_at.is_some()
            || grant.capabilities.len() != 1
        {
            return Err("local_grant_scope_rejected");
        }
        let grant_id = grant.id.clone();
        let expires_at = grant.expires_at.expect("validated expiry");
        lock(&self.inner.trusted_grants).insert(
            grant.id.clone(),
            TrustedGrant {
                grant,
                edge_derived: false,
            },
        );
        self.inner
            .authority_tx
            .send_modify(|version| *version = version.wrapping_add(1));
        self.expire_grant_at(grant_id, expires_at);
        Ok(())
    }

    pub(crate) fn reset_edge_grants(&self) {
        let mut grants = lock(&self.inner.trusted_grants);
        let previous = grants.len();
        let was_ready = self.inner.edge_grants_ready.swap(false, Ordering::AcqRel);
        grants.retain(|_, grant| !grant.edge_derived);
        if grants.len() != previous || was_ready {
            self.inner
                .authority_tx
                .send_modify(|version| *version = version.wrapping_add(1));
        }
    }

    pub(crate) fn watch_authority(&self) -> watch::Receiver<u64> {
        self.inner.authority_tx.subscribe()
    }

    /// Project only live grants for the authenticated subject and sessions
    /// already present in this chat. Shared Loro rows can never enter this set.
    pub(crate) fn collaboration_grants(
        &self,
        principal_subject: &str,
        session_ids: &[String],
    ) -> Vec<CapabilityGrant> {
        let now = now_ms();
        lock(&self.inner.trusted_grants)
            .values()
            .map(|trusted| &trusted.grant)
            .filter(|grant| {
                grant.principal_subject == principal_subject
                    && grant.granted_at <= now
                    && grant.expires_at.is_some_and(|expires| now < expires)
                    && grant.revoked_at.is_none()
                    && grant
                        .scope
                        .session_id
                        .as_ref()
                        .is_some_and(|session_id| session_ids.contains(session_id))
            })
            .cloned()
            .collect()
    }

    /// Resolve a typed local-file anchor against the chat's authenticated workspace row.
    ///
    /// The relative path has already passed collaboration schema validation. Canonicalizing both
    /// paths additionally prevents a workspace symlink from projecting text outside the checkout.
    pub(crate) fn annotation_target_text(
        &self,
        chat_id: &str,
        anchor: &SemanticAnchor,
    ) -> Option<String> {
        let FileTargetReference::LocalWorkspacePath {
            workspace_id,
            relative_path,
            ..
        } = anchor.file.as_ref()?
        else {
            return None;
        };
        let workspace = self.workspace()?;
        let chat = workspace.doc().chat(chat_id).ok()??;
        if chat.space_id.as_deref() != Some(workspace_id.as_str()) {
            return None;
        }
        let root = std::fs::canonicalize(chat.cwd?).ok()?;
        let target = std::fs::canonicalize(root.join(relative_path)).ok()?;
        if !target.starts_with(&root) {
            return None;
        }
        std::fs::read_to_string(target).ok()
    }

    pub fn device_id(&self) -> &str {
        &self.inner.config.device_id
    }

    /// The session UUID is the only global room address (edge
    /// `canonicalSessionId`, edge/src/session-room.ts): an imported local
    /// reference dialing `/session/local-chat-…/ws` hits a route the Worker
    /// can never serve, so the join loop would retry a permanent 404 forever.
    /// Those docs stay local regardless of which controller they acquire.
    fn chat_allows_room_join(&self, chat_id: &str) -> bool {
        !chat_id.starts_with("local-chat-")
    }

    fn start_room_join(&self, handle: Arc<ChatDocHandle>) {
        let Some(edge) = &self.inner.config.edge else {
            return;
        };
        if !self.chat_allows_room_join(&handle.chat_id)
            || lock(&handle.room).is_some()
            || handle
                .room_join_started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }

        let generation = handle.room_generation.load(Ordering::Acquire);
        let projection = lock(&handle.room_projection).clone();
        let url = edge.room_url_for(
            format!("/session/{}/ws", handle.chat_id),
            projection.as_ref(),
        );
        let room_doc = handle.doc.doc().clone();
        let chat = handle.chat_id.clone();
        let weak = Arc::downgrade(&handle);
        tokio::spawn(async move {
            let mut wake = comet_sync::wake::subscribe();
            let mut backoff = crate::workspace_host::JOIN_RETRY_BASE;
            loop {
                if !weak.upgrade().is_some_and(|handle| {
                    handle.room_generation.load(Ordering::Acquire) == generation
                }) {
                    return;
                }
                let cap = match RoomClient::connect_via(url.clone(), &chat, room_doc.clone()).await
                {
                    Ok(client) => {
                        let Some(handle) = weak.upgrade() else {
                            return;
                        };
                        let mut room = lock(&handle.room);
                        if handle.room_generation.load(Ordering::Acquire) != generation {
                            return;
                        }
                        *room = Some(client);
                        tracing::info!(chat = %chat, "session room joined");
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(
                            chat = %chat,
                            error = %err,
                            backoff_ms = backoff.as_millis() as u64,
                            "session room join failed; retrying"
                        );
                        join_retry_cap(&err)
                    }
                };
                tokio::select! {
                    _ = tokio::time::sleep(backoff + crate::workspace_host::join_retry_jitter()) => {
                        backoff = (backoff * 2).min(cap);
                    }
                    _ = wake.recv() => {
                        backoff = crate::workspace_host::JOIN_RETRY_BASE;
                    }
                }
            }
        });
    }

    /// Start room supervision for an already-open chat (e.g. once a native run
    /// claims it). Imported `local-chat-…` docs are never eligible — see
    /// [`Self::chat_allows_room_join`].
    pub(crate) fn ensure_room_for_chat(&self, chat_id: &str) {
        let handle = lock(&self.inner.handles).get(chat_id).cloned();
        if let Some(handle) = handle {
            self.start_room_join(handle);
        }
    }

    /// Open (or return) the chat's doc handle: load the local snapshot (or init
    /// fresh), start the change-driven task, and join an eligible edge room.
    pub fn open(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        self.open_projection(chat_id, None)
    }

    /// Return an existing projection-aware handle, or open an ordinary local
    /// handle when no controller has attached this chat yet. Ordinary opens
    /// preserve any trusted projection already installed on the handle.
    pub(crate) fn open_existing_or_local(
        &self,
        chat_id: &str,
    ) -> Result<Arc<ChatDocHandle>, EngineError> {
        let handle = lock(&self.inner.handles).get(chat_id).cloned();
        if let Some(handle) = handle {
            handle.touch();
            self.start_room_join(handle.clone());
            return Ok(handle);
        }
        self.open(chat_id)
    }

    /// Open a nudged chat without allowing a Scaffold host to join an
    /// unprojected room before its verified edge grant arrives.
    pub(crate) fn open_for_nudge(
        &self,
        chat_id: &str,
    ) -> Result<Option<Arc<ChatDocHandle>>, EngineError> {
        if let Some(handle) = lock(&self.inner.handles).get(chat_id).cloned() {
            handle.touch();
            self.start_room_join(handle.clone());
            return Ok(Some(handle));
        }
        if comet_proto::parse_scaffold_device_id(self.device_id()).is_none() {
            return self.open(chat_id).map(Some);
        }

        let now = now_ms();
        let projection = lock(&self.inner.trusted_grants)
            .values()
            .find_map(|trusted| {
                let grant = &trusted.grant;
                let deployment_id = grant.scope.deployment_id.as_deref()?;
                (trusted.edge_derived
                    && grant.scope.session_id.as_deref() == Some(chat_id)
                    && grant.device_id.as_deref() == Some(self.device_id())
                    && grant.granted_at <= now
                    && grant.expires_at.is_some_and(|expires| now < expires)
                    && grant.revoked_at.is_none())
                .then(|| SessionRoomProjection {
                    project_id: grant.scope.project_id.clone(),
                    deployment_id: deployment_id.to_string(),
                    session_id: chat_id.to_string(),
                })
            });
        let Some(projection) = projection else {
            return Ok(None);
        };
        self.open_projection(chat_id, Some(&projection)).map(Some)
    }

    /// Open a chat against an exact Scaffold room selected by the control-plane
    /// attach result. Scope is allowed only when its session id is this document's
    /// id; unscoped callers retain the ordinary local s3 projection.
    pub fn open_projection(
        &self,
        chat_id: &str,
        projection: Option<&SessionRoomProjection>,
    ) -> Result<Arc<ChatDocHandle>, EngineError> {
        if let Some(projection) = projection
            && (projection.project_id != self.workspace().map_or("", WorkspaceHost::project_scope)
                || projection.session_id != chat_id
                || projection.deployment_id.trim().is_empty())
        {
            return Err(EngineError::Other(
                "session room projection does not match local project/chat".into(),
            ));
        }
        if let Some(handle) = lock(&self.inner.handles).get(chat_id) {
            handle.ensure_room_projection(projection)?;
            handle.touch();
            self.start_room_join(handle.clone());
            return Ok(handle.clone());
        }
        let mut snapshot_len = 0usize;
        let doc = match self.inner.store.load_snapshot(chat_id)? {
            Some(bytes) => {
                snapshot_len = bytes.len();
                let raw = loro::LoroDoc::new();
                raw.import(&bytes)
                    .map_err(|e| EngineError::Other(format!("snapshot import failed: {e}")))?;
                SessionDoc::from_doc(raw)
            }
            None => SessionDoc::init(chat_id)?,
        };
        let doc = Arc::new(doc);

        let (changed_tx, changed_rx) = watch::channel(0u64);
        let sub = doc.doc().subscribe_root(Arc::new(move |_diff| {
            changed_tx.send_modify(|v| *v = v.wrapping_add(1));
        }));
        // The mirror starts dirty and empty: many opens never watch a
        // transcript, and the first attach materializes the bounded tail.
        let (messages_tx, _) = watch::channel(SessionEntryWindow {
            entries: Vec::new(),
            before: None,
        });
        let handle = Arc::new(ChatDocHandle {
            chat_id: chat_id.to_string(),
            device_id: self.inner.config.device_id.clone(),
            doc: doc.clone(),
            messages_tx,
            mirror_dirty: AtomicBool::new(true),
            last_access: AtomicI64::new(now_ms()),
            snapshot_bytes: AtomicUsize::new(snapshot_len),
            room_projection: Mutex::new(projection.cloned()),
            room: Mutex::new(None),
            room_generation: AtomicU64::new(0),
            command_lock: Mutex::new(()),
            room_join_started: AtomicBool::new(false),
            _sub: sub,
        });
        let racing = {
            let mut handles = lock(&self.inner.handles);
            if let Some(existing) = handles.get(chat_id) {
                existing.ensure_room_projection(projection)?;
                Some(existing.clone())
            } else {
                handles.insert(chat_id.to_string(), handle.clone());
                None
            }
        };
        if let Some(existing) = racing {
            self.start_room_join(existing.clone());
            return Ok(existing);
        }

        self.start_room_join(handle.clone());

        tokio::spawn(chat_task(self.clone(), Arc::downgrade(&handle), changed_rx));
        self.evict_over_budget();
        Ok(handle)
    }

    /// Bind a per-agent-session execution key to the shared thread document. SessionsEngine
    /// remains keyed by its first argument, so distinct keys run concurrently while every
    /// writer receives the same `SessionDoc` and publications CRDT-merge in one room.
    fn bind_session_execution_key(&self, handle: &Arc<ChatDocHandle>, session_id: &str) -> String {
        let key = format!("{}::session::{session_id}", handle.chat_id);
        lock(&self.inner.handles).insert(key.clone(), handle.clone());
        key
    }

    /// LRU eviction: while the warm set exceeds [`WARM_DOC_CAP`] or the
    /// resident estimate exceeds `DOC_LRU_BYTE_BUDGET`, close the
    /// least-recently-touched unpinned docs. Pinned (never evicted):
    /// - watched docs (`messages_tx` has receivers — a UI transcript);
    /// - docs with a live writer (`Arc<SessionDoc>` held outside the handle —
    ///   a run streaming into it);
    /// - host-side docs with pending commands (the executor owes them work).
    ///
    /// Eviction flushes a final snapshot, so reopen loses nothing; missed
    /// remote updates re-arrive through the room join's VV backfill.
    fn evict_over_budget(&self) {
        let mut by_age: Vec<(i64, String)> = {
            let handles = lock(&self.inner.handles);
            handles
                .iter()
                .filter(|(key, handle)| key.as_str() == handle.chat_id)
                .map(|(_, h)| (h.last_access.load(Ordering::Relaxed), h.chat_id.clone()))
                .collect()
        };
        by_age.sort_unstable();
        for (last_access, chat_id) in by_age {
            if now_ms() - last_access < EVICT_MIN_IDLE_MS {
                // Sorted oldest-first: everything after this is younger.
                return;
            }
            let (count, estimate) = {
                let handles = lock(&self.inner.handles);
                // Execution keys alias a shared handle; budget each document
                // once, not once per agent session that has used it.
                handles
                    .iter()
                    .filter(|(key, handle)| key.as_str() == handle.chat_id)
                    .fold((0, 0), |(count, bytes), (_, handle)| {
                        (count + 1, bytes + handle.resident_estimate())
                    })
            };
            if count <= WARM_DOC_CAP && estimate <= comet_doc::DOC_LRU_BYTE_BUDGET {
                return;
            }
            let evicted = {
                let mut handles = lock(&self.inner.handles);
                match handles.get(&chat_id) {
                    Some(handle) if !self.pinned(handle) => {
                        let handle = handle.clone();
                        handles.retain(|_, candidate| !Arc::ptr_eq(candidate, &handle));
                        Some(handle)
                    }
                    _ => None,
                }
            };
            if let Some(handle) = evicted {
                // Final flush outside the map lock; ≤1s of changes could be
                // pending in the snapshot debounce.
                self.save_snapshot(&handle);
                tracing::debug!(chat = %handle.chat_id, "doc evicted (LRU)");
            }
        }
    }

    fn pinned(&self, handle: &Arc<ChatDocHandle>) -> bool {
        if handle.messages_tx.receiver_count() > 0 {
            return true;
        }
        // The handle itself holds one doc ref; more means a live writer.
        if Arc::strong_count(&handle.doc) > 1 {
            return true;
        }
        if self.is_host(&handle.chat_id) {
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            match handle.doc.read_commands() {
                Ok(commands) => commands
                    .iter()
                    .any(|c| c.status == SessionCommandStatus::Pending && !is_processed(&c.id)),
                // Unreadable ledger: keep the doc, never evict blind.
                Err(_) => true,
            }
        } else {
            false
        }
    }

    /// Probe every open chat's room (window-focus liveness sweep). Each
    /// room ignores the hint unless it has been broadcast-quiet ≥30s.
    pub fn probe_open_chats(&self) {
        let handles: Vec<Arc<ChatDocHandle>> = lock(&self.inner.handles)
            .iter()
            .filter(|(key, handle)| key.as_str() == handle.chat_id)
            .map(|(_, handle)| handle.clone())
            .collect();
        for handle in handles {
            if let Some(room) = lock(&handle.room).as_ref() {
                room.probe();
            }
        }
    }

    /// Per-open-chat room introspection for SyncStatus / `comet sync`.
    /// `None` room = still dialing (join retry loop) or edge-less.
    pub fn sync_statuses(&self) -> Vec<(String, Option<comet_sync::RoomStatsSnapshot>)> {
        let handles: Vec<Arc<ChatDocHandle>> = lock(&self.inner.handles)
            .iter()
            .filter(|(key, handle)| key.as_str() == handle.chat_id)
            .map(|(_, handle)| handle.clone())
            .collect();
        let mut rows: Vec<(String, Option<comet_sync::RoomStatsSnapshot>)> = handles
            .iter()
            .map(|h| {
                (
                    h.chat_id.clone(),
                    lock(&h.room).as_ref().map(RoomClient::stats),
                )
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    /// Drop a chat's doc unconditionally and delete its local snapshot — the
    /// chat is gone (DeleteChat / DeleteSpace cascade). Watchers see the
    /// stream end; a racing writer keeps its orphaned doc until the run ends.
    pub fn purge_chat(&self, chat_id: &str) {
        lock(&self.inner.handles).retain(|_, handle| handle.chat_id != chat_id);
        if let Err(err) = self.inner.store.delete_snapshot(chat_id) {
            tracing::warn!(chat = %chat_id, error = %err, "snapshot delete failed");
        }
    }

    /// An authenticated, host-local grant may persist authority for the exact
    /// immutable command it authorized. Relay-derived grants never cross this
    /// seam: they must be freshly ingested by the owning process.
    fn local_owner_grant_authorizes(&self, entry: &SessionCommandEntry) -> bool {
        let SessionCommandPayload::Control {
            owner_device_id,
            actor_device_id,
            source,
            grant_id,
            ..
        } = &entry.payload
        else {
            return false;
        };
        if entry.issued_by != self.device_id()
            || owner_device_id != self.device_id()
            || actor_device_id != self.device_id()
            || !matches!(source, comet_proto::AgentSessionSource::Local)
        {
            return false;
        }
        let Some(workspace) = self.workspace() else {
            return false;
        };
        lock(&self.inner.trusted_grants)
            .get(grant_id)
            .is_some_and(|trusted| {
                !trusted.edge_derived
                    && grant_authorizes_control_entry(
                        &trusted.grant,
                        entry,
                        workspace.project_scope(),
                        now_ms(),
                    )
            })
    }

    fn edge_grant_authorizes_local_scaffold_command(
        &self,
        chat_id: &str,
        projection: Option<&SessionRoomProjection>,
        entry: &SessionCommandEntry,
    ) -> bool {
        let SessionCommandPayload::Control {
            actor_device_id,
            owner_device_id,
            grant_id,
            source,
            ..
        } = &entry.payload
        else {
            return false;
        };
        if entry.issued_by != self.device_id()
            || actor_device_id == self.device_id()
            || owner_device_id != self.device_id()
            || !matches!(source, comet_proto::AgentSessionSource::Scaffold)
        {
            return false;
        }
        let Some(workspace) = self.workspace() else {
            return false;
        };
        lock(&self.inner.trusted_grants)
            .get(grant_id)
            .is_some_and(|trusted| {
                trusted.edge_derived
                    && grant_authorizes_control_scope(
                        &trusted.grant,
                        entry,
                        workspace.project_scope(),
                        now_ms(),
                    )
                    && grant_matches_projected_scaffold_room(&trusted.grant, chat_id, projection)
            })
    }

    fn command_nudge_route(
        &self,
        chat_id: &str,
        projection: Option<&SessionRoomProjection>,
        entry: &SessionCommandEntry,
    ) -> CommandNudgeRoute {
        let SessionCommandPayload::Control {
            actor_device_id,
            owner_device_id,
            grant_id,
            source,
            ..
        } = &entry.payload
        else {
            return CommandNudgeRoute::WorkspaceHost;
        };
        if entry.issued_by != self.device_id()
            || actor_device_id != self.device_id()
            || owner_device_id == self.device_id()
        {
            return CommandNudgeRoute::None;
        }
        if !matches!(source, comet_proto::AgentSessionSource::Scaffold) {
            return CommandNudgeRoute::WorkspaceHost;
        }
        let Some(workspace) = self.workspace() else {
            return CommandNudgeRoute::None;
        };
        let grants = lock(&self.inner.trusted_grants);
        let Some(trusted) = grants.get(grant_id) else {
            return CommandNudgeRoute::None;
        };
        if trusted.edge_derived
            || trusted.grant.granted_by != "comet-edge-device-room"
            || !grant_authorizes_control_entry(
                &trusted.grant,
                entry,
                workspace.project_scope(),
                now_ms(),
            )
            || !grant_matches_projected_scaffold_room(&trusted.grant, chat_id, projection)
        {
            return CommandNudgeRoute::None;
        }
        trusted
            .grant
            .device_id
            .clone()
            .map(CommandNudgeRoute::ExactDevice)
            .unwrap_or(CommandNudgeRoute::None)
    }

    /// Composer path: append an immutable pending command entry (rule 1). Durable by
    /// construction — the change subscription kicks the drain, so a local host executes
    /// immediately and an offline doc simply holds the entry until it syncs.
    pub fn queue_command(
        &self,
        chat_id: &str,
        payload: SessionCommandPayload,
    ) -> Result<String, EngineError> {
        let id = new_id();
        self.queue_command_with_id(chat_id, &id, payload)?;
        Ok(id)
    }

    /// Append using a caller-supplied durable id. Retries return the immutable
    /// existing command instead of adding or replacing a ledger entry.
    pub fn queue_command_with_id(
        &self,
        chat_id: &str,
        command_id: &str,
        payload: SessionCommandPayload,
    ) -> Result<SessionCommandEntry, EngineError> {
        let existing = { lock(&self.inner.handles).get(chat_id).cloned() };
        let handle = match existing {
            Some(handle) => {
                handle.touch();
                handle
            }
            None => self.open(chat_id)?,
        };
        let _guard = lock(&handle.command_lock);
        if let Some(existing) = handle
            .doc
            .read_commands()?
            .into_iter()
            .find(|entry| entry.id == command_id)
        {
            return Ok(existing);
        }
        let now = now_ms();
        let based_on = handle.doc.read_entries()?.last().map(|m| CommandBasedOn {
            turn_id: Some(m.id.clone()),
            frontier: None,
        });
        let entry = SessionCommandEntry {
            id: command_id.to_string(),
            payload,
            issued_by: self.inner.config.device_id.clone(),
            issued_at: now,
            based_on,
            expires_at: Some(now + COMMAND_DEFAULT_TTL_MS),
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        let projection = lock(&handle.room_projection).clone();
        let nudge_route = self.command_nudge_route(chat_id, projection.as_ref(), &entry);
        let authorized_local_scaffold_command =
            self.edge_grant_authorizes_local_scaffold_command(chat_id, projection.as_ref(), &entry);
        if matches!(
            &entry.payload,
            SessionCommandPayload::Control {
                source: comet_proto::AgentSessionSource::Scaffold,
                ..
            }
        ) && !matches!(nudge_route, CommandNudgeRoute::ExactDevice(_))
            && !authorized_local_scaffold_command
        {
            return Err(EngineError::Other(
                "Scaffold control command does not match its attached room".into(),
            ));
        }
        let locally_authorized_control = self.local_owner_grant_authorizes(&entry);
        let locally_trusted_control =
            locally_authorized_control || authorized_local_scaffold_command;
        let trust_key = if matches!(&entry.payload, SessionCommandPayload::Control { .. }) {
            locally_trusted_control
                .then(|| local_owner_authority_key(&entry))
                .transpose()
                .map_err(|error| {
                    EngineError::Other(format!("local control authority serialize: {error}"))
                })?
        } else {
            Some(command_id.to_string())
        };
        if let Some(trust_key) = trust_key.as_deref() {
            self.inner.store.trust_local_command(trust_key)?;
        }
        if let Err(err) = handle.doc.queue_command(&entry) {
            if let Some(trust_key) = trust_key.as_deref() {
                let _ = self.inner.store.forget_local_command(trust_key);
            }
            return Err(err.into());
        }
        let explicit_host = match nudge_route {
            CommandNudgeRoute::None => None,
            CommandNudgeRoute::WorkspaceHost => None,
            CommandNudgeRoute::ExactDevice(device_id) => Some(device_id),
        };
        self.nudge_remote_host(chat_id, explicit_host.as_deref());
        Ok(entry)
    }

    pub fn command_entry(
        &self,
        chat_id: &str,
        command_id: &str,
    ) -> Result<Option<SessionCommandEntry>, EngineError> {
        let handle = self.open(chat_id)?;
        let _guard = lock(&handle.command_lock);
        Ok(handle
            .doc
            .read_commands()?
            .into_iter()
            .find(|entry| entry.id == command_id))
    }

    pub fn chat_has_commands(&self, chat_id: &str) -> Result<bool, EngineError> {
        let handle = self.open(chat_id)?;
        let _guard = lock(&handle.command_lock);
        Ok(!handle.doc.read_commands()?.is_empty())
    }

    /// POST `{edge}/device/{host}/nudge {chatId}`. Versioned control commands
    /// carry the remote owner explicitly; legacy commands fall back to the
    /// workspace chat row. Offline and edge-less engines skip silently.
    fn nudge_remote_host(&self, chat_id: &str, explicit_host: Option<&str>) {
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let host_device = explicit_host
            .filter(|device| !device.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.workspace()
                    .and_then(|workspace| workspace.doc().chat(chat_id).ok().flatten())
                    .map(|chat| chat.device_id)
            });
        let Some(host_device) = host_device else {
            return;
        };
        if host_device == self.inner.config.device_id {
            return;
        }
        // Only meaningful inside a runtime (RPC handlers, executors); bare sync
        // callers (unit tests) skip rather than panic.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let url = format!(
            "{}/device/{}/nudge",
            edge.url.trim_end_matches('/'),
            host_device
        );
        let chat = chat_id.to_string();
        runtime.spawn(async move {
            const RETRY_DELAYS_MS: [u64; 5] = [0, 250, 1_000, 3_000, 7_000];
            let http = reqwest::Client::new();
            for (attempt, delay_ms) in RETRY_DELAYS_MS.into_iter().enumerate() {
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                // Fresh bearer per attempt — never the boot-time snapshot.
                let Some(bearer) = edge.bearer().await else {
                    if attempt + 1 == RETRY_DELAYS_MS.len() {
                        tracing::warn!(chat = %chat, "nudge skipped: signed out");
                    }
                    continue;
                };
                let send = http
                    .post(&url)
                    .bearer_auth(&bearer)
                    .json(&serde_json::json!({ "chatId": chat }))
                    .timeout(std::time::Duration::from_secs(10))
                    .send()
                    .await;
                match send {
                    Ok(response) if response.status().is_success() => {
                        tracing::info!(
                            chat = %chat,
                            device = %host_device,
                            attempts = attempt + 1,
                            "host nudged"
                        );
                        return;
                    }
                    Ok(response) if attempt + 1 == RETRY_DELAYS_MS.len() => {
                        tracing::warn!(
                            chat = %chat,
                            device = %host_device,
                            status = response.status().as_u16(),
                            "nudge rejected after retries"
                        );
                    }
                    Err(err) if attempt + 1 == RETRY_DELAYS_MS.len() => {
                        tracing::warn!(
                            chat = %chat,
                            device = %host_device,
                            error = %err,
                            "nudge failed after retries"
                        );
                    }
                    _ => {}
                }
            }
        });
    }

    /// Legacy chat ownership for pre-v2 commands.
    fn is_host(&self, chat_id: &str) -> bool {
        self.workspace().is_none_or(|ws| ws.is_host(chat_id))
    }

    /// Strict ownership check for live waits: unlike `is_host`, a missing chat row
    /// is not enough to claim that this engine hosts the source session.
    pub fn is_locally_hosted(&self, chat_id: &str) -> bool {
        self.workspace().is_none_or(|workspace| {
            workspace
                .doc()
                .chat(chat_id)
                .ok()
                .flatten()
                .is_some_and(|chat| chat.device_id == workspace.device_id())
        })
    }

    /// Chat-config harness when the workspace row carries one, else the default.
    pub(crate) fn harness_for(&self, chat_id: &str) -> HarnessId {
        self.workspace()
            .and_then(|ws| ws.chat_config(chat_id))
            .map(|config| config.harness)
            .unwrap_or(self.inner.config.default_harness)
    }

    fn owns_command(&self, chat_id: &str, entry: &SessionCommandEntry) -> bool {
        entry.session_owner().map_or_else(
            || self.is_host(chat_id),
            |(_, owner)| owner == self.device_id(),
        )
    }

    /// Validate against a grant previously ingested from the control plane. Command-carried
    /// actor/scope strings are lookup inputs only and never confer authority themselves.
    /// `None` means relay authority is refreshing, so the command must remain pending.
    fn command_grant_authorization(&self, entry: &SessionCommandEntry) -> Option<bool> {
        let SessionCommandPayload::Control {
            actor_device_id,
            owner_device_id,
            grant_id,
            source,
            ..
        } = &entry.payload
        else {
            // Run/steer/input commands authored by this device's local API carry
            // durable device-local provenance. A synced peer can copy `issuedBy`
            // in Loro, but cannot write this host's SQLite trust ledger.
            return Some(
                entry.issued_by == self.device_id()
                    && self
                        .inner
                        .store
                        .is_trusted_local_command(&entry.id)
                        .unwrap_or(false),
            );
        };
        let Some(workspace) = self.workspace() else {
            return Some(false);
        };
        {
            let grants = lock(&self.inner.trusted_grants);
            if matches!(source, comet_proto::AgentSessionSource::Scaffold)
                && !self.inner.edge_grants_ready.load(Ordering::Acquire)
            {
                return None;
            }
            // A present grant is authoritative even when it is stale or revoked:
            // never fall through to restart reconstruction and bypass that state.
            if let Some(trusted) = grants.get(grant_id) {
                let now = now_ms();
                if trusted.edge_derived
                    && entry.issued_by == self.device_id()
                    && local_owner_authority_key(entry).is_ok_and(|key| {
                        self.inner
                            .store
                            .is_trusted_local_command(&key)
                            .unwrap_or(false)
                    })
                {
                    return Some(grant_authorizes_control_scope(
                        &trusted.grant,
                        entry,
                        workspace.project_scope(),
                        now,
                    ));
                }
                return Some(grant_authorizes_control_entry(
                    &trusted.grant,
                    entry,
                    workspace.project_scope(),
                    now,
                ));
            }
        }
        // Only authenticated local-owner commands receive this exact immutable
        // fingerprint at queue time. The bounded reconstruction window matches
        // the local grant TTL; remote grants always fail closed after restart.
        let now = now_ms();
        Some(
            entry.issued_by == self.device_id()
                && owner_device_id == self.device_id()
                && actor_device_id == self.device_id()
                && matches!(source, comet_proto::AgentSessionSource::Local)
                && entry.issued_at <= now
                && now < entry.issued_at.saturating_add(LOCAL_OWNER_GRANT_TTL_MS)
                && entry.expires_at.is_none_or(|expires_at| now < expires_at)
                && local_owner_authority_key(entry).is_ok_and(|key| {
                    self.inner
                        .store
                        .is_trusted_local_command(&key)
                        .unwrap_or(false)
                }),
        )
    }

    /// Imported session refs are the explicit opt-in for ordinary project
    /// collaborators to send chat-shaped commands through the shared room.
    /// Keep this fallback narrow: run, input, interrupt, environment, and
    /// control actions still require device-local provenance or a verified
    /// capability grant.
    fn shared_session_chat_authorized(&self, chat_id: &str, entry: &SessionCommandEntry) -> bool {
        if entry.issued_by == self.device_id()
            || !matches!(
                &entry.payload,
                SessionCommandPayload::Steer { .. }
                    | SessionCommandPayload::Queue { .. }
                    | SessionCommandPayload::PeerMessage { .. }
            )
        {
            return false;
        }
        self.workspace().is_some_and(|workspace| {
            workspace.doc().read_session_refs().is_ok_and(|refs| {
                refs.iter()
                    .any(|session_ref| session_ref.chat_id == chat_id)
            })
        })
    }

    fn command_grant_authorization_for_chat(
        &self,
        chat_id: &str,
        entry: &SessionCommandEntry,
    ) -> Option<bool> {
        match self.command_grant_authorization(entry) {
            Some(false) if self.shared_session_chat_authorized(chat_id, entry) => Some(true),
            authorization => authorization,
        }
    }
    #[cfg(test)]
    fn command_grant_authorized(&self, entry: &SessionCommandEntry) -> bool {
        self.command_grant_authorization(entry) == Some(true)
    }

    /// Drain only commands owned by this device. Other owners drain their own session
    /// commands from the same shared document concurrently.
    pub async fn drain_commands(&self, handle: &Arc<ChatDocHandle>) {
        let Some(sessions) = self.inner.sessions.get() else {
            return;
        };
        let mut skipped: HashSet<String> = HashSet::new();
        loop {
            let commands = match handle.doc.read_commands() {
                Ok(commands) => commands,
                Err(err) => {
                    tracing::warn!(chat = %handle.chat_id, error = %err, "command read failed");
                    return;
                }
            };
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            let Some(entry) = commands
                .iter()
                .find(|command| {
                    command.status == SessionCommandStatus::Pending
                        && !skipped.contains(&command.id)
                        && !is_processed(&command.id)
                        && self.owns_command(&handle.chat_id, command)
                })
                .cloned()
            else {
                return;
            };
            let messages = handle.doc.read_entries().unwrap_or_default();
            let current_turn_id = messages.last().map(|message| message.id.clone());
            let turn_is_past = |turn_id: &str| messages.iter().any(|message| message.id == turn_id);
            let disposition = evaluate_command(
                &entry,
                &EvaluationContext {
                    is_processed: &is_processed,
                    now_ms: now_ms(),
                    entries: &commands,
                    current_turn_id: current_turn_id.as_deref(),
                    turn_is_past: &turn_is_past,
                },
            );
            let grant_authorization = match disposition {
                CommandDisposition::Execute => {
                    self.command_grant_authorization_for_chat(&handle.chat_id, &entry)
                }
                _ => Some(true),
            };
            if grant_authorization.is_none() {
                skipped.insert(entry.id.clone());
                continue;
            }
            if let Err(err) = self.inner.store.mark_processed(&entry.id) {
                tracing::error!(chat = %handle.chat_id, error = %err,
                    "processed-ledger write failed; halting drain");
                return;
            }
            match disposition {
                CommandDisposition::Skip => {
                    skipped.insert(entry.id.clone());
                }
                CommandDisposition::Expired => {
                    self.resolve_command(handle, &entry, SessionCommandStatus::Expired, None);
                }
                CommandDisposition::Superseded => {
                    self.resolve_command(handle, &entry, SessionCommandStatus::Superseded, None);
                }
                CommandDisposition::Execute => {
                    let (status, resolution) = if grant_authorization != Some(true) {
                        (
                            SessionCommandStatus::Rejected,
                            Some("verified capability grant does not authorize command".into()),
                        )
                    } else {
                        match self.execute(sessions, handle, &entry).await {
                            Ok(outcome) => outcome,
                            Err(err) => {
                                tracing::warn!(chat = %handle.chat_id, command = %entry.id, error = %err, "command execution rejected");
                                (SessionCommandStatus::Rejected, Some(err.to_string()))
                            }
                        }
                    };
                    self.resolve_command(handle, &entry, status, resolution.as_deref());
                }
            }
            if let Err(err) = self.inner.store.forget_local_command(&entry.id) {
                tracing::debug!(command = %entry.id, error = %err, "local command trust cleanup failed");
            }
            if matches!(&entry.payload, SessionCommandPayload::Control { .. })
                && let Ok(key) = local_owner_authority_key(&entry)
                && let Err(err) = self.inner.store.forget_local_command(&key)
            {
                tracing::debug!(command = %entry.id, error = %err, "local control authority cleanup failed");
            }
        }
    }

    /// The owning device is the sole outcome writer. Audit ids are command-derived, so
    /// reconnect/retry remains idempotent.
    fn resolve_command(
        &self,
        handle: &ChatDocHandle,
        entry: &SessionCommandEntry,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) {
        if let Err(err) = handle.doc.set_command_status(&entry.id, status, resolution) {
            tracing::warn!(chat = %handle.chat_id, command = %entry.id, error = %err,
                "command outcome write failed");
        }
        let (actor_device_id, target_id) = match &entry.payload {
            SessionCommandPayload::Control {
                actor_device_id,
                session_id,
                action,
                ..
            } => {
                let target_id = match action.as_ref() {
                    SessionControlAction::AnnotationCreate { annotation } => annotation.id.clone(),
                    SessionControlAction::AnnotationEdit { annotation_id, .. }
                    | SessionControlAction::AnnotationResolve { annotation_id, .. } => {
                        annotation_id.clone()
                    }
                    _ => session_id.clone(),
                };
                let target_id = if target_id.is_empty() || target_id.len() > 256 {
                    session_id.clone()
                } else {
                    target_id
                };
                (actor_device_id.clone(), target_id)
            }
            _ => (entry.issued_by.clone(), handle.chat_id.clone()),
        };
        let result = match status {
            SessionCommandStatus::Applied => AuditResult::Applied,
            SessionCommandStatus::Rejected => AuditResult::Rejected,
            SessionCommandStatus::Expired => AuditResult::Expired,
            SessionCommandStatus::Superseded => AuditResult::Superseded,
            SessionCommandStatus::Cancelled => AuditResult::Cancelled,
            SessionCommandStatus::Pending => return,
        };
        let at = now_ms();
        let audit_id = format!("audit/{}", entry.id);
        let audit = PublicationRecord {
            id: audit_id.clone(),
            schema_version: COLLABORATION_SCHEMA_VERSION,
            published_at: at,
            published_by: entry.actor_subject().to_string(),
            value: PublicationValue::Audit(AuditEvent {
                id: audit_id,
                actor_subject: entry.actor_subject().to_string(),
                actor_device_id,
                target_id,
                action: entry.action_name().to_string(),
                occurred_at: at,
                result,
                reason: resolution.map(str::to_string),
                unknown: Default::default(),
            }),
            unknown: Default::default(),
        };
        if let Err(err) = handle.doc.append_publication(&audit) {
            tracing::warn!(chat = %handle.chat_id, command = %entry.id, error = %err,
                "command audit publication failed");
        }
        if status == SessionCommandStatus::Applied
            && let SessionCommandPayload::Control {
                session_id, action, ..
            } = &entry.payload
            && let Some(next_status) = match action.as_ref() {
                SessionControlAction::Pause {} | SessionControlAction::Stop {} => {
                    Some(comet_proto::SessionStatus::Idle)
                }
                _ => None,
            }
            && let Ok(snapshot) = handle.doc.collaboration_snapshot()
            && let Some(mut session) = snapshot
                .sessions
                .into_iter()
                .find(|session| session.session_id == *session_id)
        {
            session.status = Some(next_status);
            session.updated_at = Some(at);
            let state = PublicationRecord {
                id: format!("session/{session_id}/command/{}", entry.id),
                schema_version: COLLABORATION_SCHEMA_VERSION,
                published_at: at,
                published_by: entry.actor_subject().to_string(),
                value: PublicationValue::AgentSession(Box::new(session)),
                unknown: Default::default(),
            };
            if let Err(err) = handle.doc.append_publication(&state) {
                tracing::warn!(chat = %handle.chat_id, command = %entry.id, error = %err,
                    "session state publication failed");
            }
        }
    }
    async fn execute(
        &self,
        sessions: &SessionsEngine,
        handle: &Arc<ChatDocHandle>,
        entry: &SessionCommandEntry,
    ) -> Result<(SessionCommandStatus, Option<String>), EngineError> {
        let chat_id = &handle.chat_id;
        let carries_user_input = match &entry.payload {
            SessionCommandPayload::Run { .. }
            | SessionCommandPayload::Steer { .. }
            | SessionCommandPayload::Queue { .. } => true,
            SessionCommandPayload::Control { action, .. } => matches!(
                action.as_ref(),
                SessionControlAction::Start { .. }
                    | SessionControlAction::Steer { .. }
                    | SessionControlAction::Queue { .. }
            ),
            _ => false,
        };
        if carries_user_input
            && let Some(workspace) = self.workspace()
            && workspace
                .doc()
                .chat(chat_id)?
                .is_some_and(|chat| chat.archived)
        {
            workspace.set_chat_archived(chat_id, false)?;
        }
        match &entry.payload {
            SessionCommandPayload::Run {
                request,
                message_id,
            } => {
                // Claim-on-first-command: a run for a chat with no workspace row
                // creates the row under our device id (we are about to host it).
                if let Some(ws) = self.workspace() {
                    ws.claim_chat(chat_id, Some(&request.cwd))?;
                }
                let harness = self.harness_for(chat_id);
                sessions
                    .dispatch(chat_id, harness, request.clone(), Some(message_id.clone()))
                    .await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::Steer { prompt, message_id } => {
                self.execute_steer(sessions, chat_id, chat_id, prompt, message_id)
                    .await
            }
            SessionCommandPayload::Queue { prompt, message_id } => {
                self.execute_queue(sessions, chat_id, chat_id, prompt, message_id)
                    .await
            }
            SessionCommandPayload::PeerMessage {
                text,
                source_chat_id,
                thread_id,
                reply_to,
                ..
            } => {
                let prompt =
                    peer_message_prompt(source_chat_id, thread_id, chat_id, &entry.id, text);
                if reply_to.is_some()
                    && let Some(waiter) = sessions.claim_peer_waiter(chat_id, thread_id)
                    && !waiter.is_closed()
                {
                    handle.write_user_message_with_status(
                        &entry.id,
                        &prompt,
                        entry.issued_at,
                        MessageStatus::Complete,
                    )?;
                    if waiter.deliver(PeerReply {
                        command_id: entry.id.clone(),
                        text: text.clone(),
                        source_chat_id: source_chat_id.clone(),
                    }) {
                        return Ok((
                            SessionCommandStatus::Applied,
                            Some("delivered to live peer waiter".into()),
                        ));
                    }
                }
                match sessions
                    .steer(chat_id, &prompt, Some(entry.id.clone()))
                    .await?
                {
                    SteerOutcome::Accepted(_) => Ok((SessionCommandStatus::Applied, None)),
                    SteerOutcome::NotSteerable => {
                        let request = sessions
                            .last_request(chat_id)
                            .or_else(|| self.request_from_chat_row(chat_id, &prompt));
                        let Some(mut request) = request else {
                            return Ok((
                                SessionCommandStatus::Rejected,
                                Some("no live run and no prior run config".into()),
                            ));
                        };
                        request.prompt = prompt;
                        request.resume = None;
                        request.attachments = Vec::new();
                        sessions
                            .dispatch(
                                chat_id,
                                self.harness_for(chat_id),
                                request,
                                Some(entry.id.clone()),
                            )
                            .await?;
                        Ok((
                            SessionCommandStatus::Applied,
                            Some("queued as new turn".into()),
                        ))
                    }
                }
            }
            SessionCommandPayload::Interrupt {} => {
                sessions.interrupt(chat_id).await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::RespondInput {
                request_id,
                answers,
            } => {
                self.execute_respond_input(sessions, handle, chat_id, request_id, answers)
                    .await
            }
            SessionCommandPayload::Control {
                session_id,
                owner_device_id,
                actor_subject,
                source,
                action,
                ..
            } => {
                if owner_device_id != self.device_id() {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("command addressed to another session owner".into()),
                    ));
                }
                let execution_key = self.bind_session_execution_key(handle, session_id);
                match action.as_ref() {
                    SessionControlAction::Start {
                        request,
                        message_id,
                    } => {
                        let at = now_ms();
                        let harness = self.harness_for(chat_id);
                        let previous =
                            handle
                                .doc
                                .collaboration_snapshot()
                                .ok()
                                .and_then(|snapshot| {
                                    snapshot
                                        .sessions
                                        .into_iter()
                                        .find(|session| session.session_id == *session_id)
                                });
                        if let Some(previous) = &previous
                            && previous.model != request.model
                        {
                            let handoff_id = format!("handoff/{session_id}/{}", entry.id);
                            let handoff = PublicationRecord {
                                id: handoff_id.clone(),
                                schema_version: COLLABORATION_SCHEMA_VERSION,
                                published_at: at,
                                published_by: actor_subject.clone(),
                                value: PublicationValue::ModelHandoff(ModelHandoff {
                                    id: handoff_id,
                                    from_model: previous.model.clone().unwrap_or_default(),
                                    to_model: request.model.clone().unwrap_or_default(),
                                    harness_session_id: previous.harness_session_id.clone(),
                                    after_publication_id: handle
                                        .doc
                                        .read_publications()
                                        .ok()
                                        .and_then(|rows| rows.last().map(|row| row.id.clone())),
                                    created_by: actor_subject.clone(),
                                    created_at: at,
                                    unknown: Default::default(),
                                }),
                                unknown: Default::default(),
                            };
                            handle.doc.append_publication(&handoff)?;
                        }
                        let session_record = AgentSessionRecord {
                            session_id: session_id.clone(),
                            chat_id: chat_id.clone(),
                            owner_subject: actor_subject.clone(),
                            owner_device_id: owner_device_id.clone(),
                            source: *source,
                            environment: None,
                            harness: Some(harness),
                            model: request.model.clone(),
                            harness_session_id: previous
                                .as_ref()
                                .and_then(|session| session.harness_session_id.clone()),
                            status: Some(comet_proto::SessionStatus::Working),
                            updated_at: Some(at),
                            created_at: previous.as_ref().map_or(at, |session| session.created_at),
                            unknown: Default::default(),
                        };
                        let session_publication = PublicationRecord {
                            id: format!("session/{session_id}/start/{}", entry.id),
                            schema_version: COLLABORATION_SCHEMA_VERSION,
                            published_at: at,
                            published_by: actor_subject.clone(),
                            value: PublicationValue::AgentSession(Box::new(session_record.clone())),
                            unknown: Default::default(),
                        };
                        handle.doc.append_publication(&session_publication)?;
                        let provenance = PublicationRecord {
                            id: format!("provenance/{message_id}"),
                            schema_version: COLLABORATION_SCHEMA_VERSION,
                            published_at: at,
                            published_by: actor_subject.clone(),
                            value: PublicationValue::MessageProvenance(MessageProvenance {
                                message_id: message_id.clone(),
                                session_id: session_id.clone(),
                                author_subject: actor_subject.clone(),
                                owner_device_id: owner_device_id.clone(),
                                model: request.model.clone(),
                                source: *source,
                                unknown: Default::default(),
                            }),
                            unknown: Default::default(),
                        };
                        handle.doc.append_publication(&provenance)?;
                        if let Err(error) = sessions
                            .dispatch(
                                &execution_key,
                                harness,
                                request.clone(),
                                Some(message_id.clone()),
                            )
                            .await
                        {
                            let failed_at = now_ms();
                            let mut failed_session = session_record;
                            failed_session.status = Some(comet_proto::SessionStatus::Errored);
                            failed_session.updated_at = Some(failed_at);
                            handle.doc.append_publication(&PublicationRecord {
                                id: format!("session/{session_id}/failed/{}", entry.id),
                                schema_version: COLLABORATION_SCHEMA_VERSION,
                                published_at: failed_at,
                                published_by: actor_subject.clone(),
                                value: PublicationValue::AgentSession(Box::new(failed_session)),
                                unknown: Default::default(),
                            })?;
                            return Err(error);
                        }
                        monitor_agent_session_terminal(
                            sessions,
                            handle.clone(),
                            execution_key.clone(),
                            session_id.clone(),
                            actor_subject.clone(),
                            entry.id.clone(),
                        );
                        Ok((SessionCommandStatus::Applied, None))
                    }
                    SessionControlAction::Steer { prompt, message_id } => {
                        self.execute_steer(sessions, &execution_key, chat_id, prompt, message_id)
                            .await
                    }
                    SessionControlAction::Queue { prompt, message_id } => {
                        self.execute_queue(sessions, &execution_key, chat_id, prompt, message_id)
                            .await
                    }
                    SessionControlAction::RespondInput {
                        request_id,
                        answers,
                    } => {
                        self.execute_respond_input(
                            sessions,
                            handle,
                            &execution_key,
                            request_id,
                            answers,
                        )
                        .await
                    }
                    SessionControlAction::Pause {} => {
                        sessions.interrupt(&execution_key).await?;
                        Ok((SessionCommandStatus::Applied, Some("paused".into())))
                    }
                    SessionControlAction::Resume {} => {
                        let Some(mut request) = sessions.last_request(&execution_key) else {
                            return Ok((
                                SessionCommandStatus::Rejected,
                                Some("target session has no resumable request".into()),
                            ));
                        };
                        request.prompt = "Continue from where this session was paused.".into();
                        request.attachments.clear();
                        sessions
                            .dispatch(&execution_key, self.harness_for(chat_id), request, None)
                            .await?;
                        append_agent_session_status(
                            handle,
                            session_id,
                            SessionStatus::Working,
                            actor_subject,
                            format!("session/{session_id}/resume/{}", entry.id),
                        )?;
                        monitor_agent_session_terminal(
                            sessions,
                            handle.clone(),
                            execution_key.clone(),
                            session_id.clone(),
                            actor_subject.clone(),
                            entry.id.clone(),
                        );
                        Ok((SessionCommandStatus::Applied, Some("resumed".into())))
                    }
                    SessionControlAction::Stop {} => {
                        sessions.interrupt(&execution_key).await?;
                        Ok((SessionCommandStatus::Applied, None))
                    }
                    SessionControlAction::Focus { .. } => {
                        // Focus is a durable collaboration command; the selected target is
                        // consumed by UI presence projection and the host records the audit.
                        Ok((SessionCommandStatus::Applied, None))
                    }
                    SessionControlAction::AnnotationCreate { annotation } => {
                        if handle.doc.read_publications()?.iter().any(|publication| {
                            matches!(
                                &publication.value,
                                PublicationValue::Annotation(existing)
                                    if existing.id == annotation.id
                            )
                        }) {
                            return Ok((
                                SessionCommandStatus::Rejected,
                                Some("annotation id already exists".into()),
                            ));
                        }
                        let mut revision = annotation.clone();
                        revision.author_subject = actor_subject.clone();
                        revision.created_at = now_ms();
                        revision.state = comet_proto::AnnotationState::Anchored;
                        revision.resolved_at = None;
                        let publication = PublicationRecord {
                            id: format!("annotation/{}/create/{}", revision.id, entry.id),
                            schema_version: COLLABORATION_SCHEMA_VERSION,
                            published_at: revision.created_at,
                            published_by: actor_subject.clone(),
                            value: PublicationValue::Annotation(revision),
                            unknown: Default::default(),
                        };
                        handle.doc.append_publication(&publication)?;
                        Ok((SessionCommandStatus::Applied, None))
                    }
                    SessionControlAction::AnnotationEdit {
                        annotation_id,
                        body,
                        anchor,
                    } => {
                        let publications = handle.doc.read_publications()?;
                        let mut revision = match annotation_revision_for_subject(
                            &publications,
                            annotation_id,
                            actor_subject,
                        ) {
                            Ok(revision) => revision,
                            Err(AnnotationMutationError::NotFound) => {
                                return Ok((
                                    SessionCommandStatus::Rejected,
                                    Some("annotation not found".into()),
                                ));
                            }
                            Err(AnnotationMutationError::NotAuthor) => {
                                return Ok((
                                    SessionCommandStatus::Rejected,
                                    Some("only the annotation author can edit it".into()),
                                ));
                            }
                        };
                        if let Some(body) = body {
                            revision.body = body.clone();
                        }
                        if let Some(anchor) = anchor {
                            revision.anchor = anchor.clone();
                        }
                        let at = now_ms();
                        let publication = PublicationRecord {
                            id: format!("annotation/{annotation_id}/edit/{}", entry.id),
                            schema_version: COLLABORATION_SCHEMA_VERSION,
                            published_at: at,
                            published_by: actor_subject.clone(),
                            value: PublicationValue::Annotation(revision),
                            unknown: Default::default(),
                        };
                        handle.doc.append_publication(&publication)?;
                        Ok((SessionCommandStatus::Applied, None))
                    }
                    SessionControlAction::AnnotationResolve {
                        annotation_id,
                        resolved,
                    } => {
                        let publications = handle.doc.read_publications()?;
                        let mut revision = match annotation_revision_for_subject(
                            &publications,
                            annotation_id,
                            actor_subject,
                        ) {
                            Ok(revision) => revision,
                            Err(AnnotationMutationError::NotFound) => {
                                return Ok((
                                    SessionCommandStatus::Rejected,
                                    Some("annotation not found".into()),
                                ));
                            }
                            Err(AnnotationMutationError::NotAuthor) => {
                                return Ok((
                                    SessionCommandStatus::Rejected,
                                    Some("only the annotation author can resolve it".into()),
                                ));
                            }
                        };
                        let at = now_ms();
                        // Resolution is orthogonal to anchor health. Preserve whether the
                        // target is anchored, re-anchored, or orphaned when closing/reopening.
                        revision.resolved_at = (*resolved).then_some(at);
                        let publication = PublicationRecord {
                            id: format!("annotation/{annotation_id}/resolve/{}", entry.id),
                            schema_version: COLLABORATION_SCHEMA_VERSION,
                            published_at: at,
                            published_by: actor_subject.clone(),
                            value: PublicationValue::Annotation(revision),
                            unknown: Default::default(),
                        };
                        handle.doc.append_publication(&publication)?;
                        Ok((SessionCommandStatus::Applied, None))
                    }
                    SessionControlAction::EnvironmentLifecycle { .. } => Ok((
                        SessionCommandStatus::Rejected,
                        Some("environment lifecycle is unavailable on this device".into()),
                    )),
                }
            }
        }
    }

    async fn execute_steer(
        &self,
        sessions: &SessionsEngine,
        execution_key: &str,
        chat_id: &str,
        prompt: &str,
        message_id: &Option<String>,
    ) -> Result<(SessionCommandStatus, Option<String>), EngineError> {
        match sessions
            .steer(execution_key, prompt, message_id.clone())
            .await?
        {
            SteerOutcome::Accepted(MessageStatus::Queued) => Ok((
                SessionCommandStatus::Applied,
                Some("queued for the next turn boundary".into()),
            )),
            SteerOutcome::Accepted(_) => Ok((SessionCommandStatus::Applied, None)),
            SteerOutcome::NotSteerable => {
                // No live steerable run: the durable command still delivers to
                // the selected session as its next turn.
                let request = sessions
                    .last_request(execution_key)
                    .or_else(|| self.request_from_chat_row(chat_id, prompt));
                let Some(mut request) = request else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no live run and no prior run config".into()),
                    ));
                };
                request.prompt = prompt.to_string();
                request.resume = None;
                request.attachments.clear();
                sessions
                    .dispatch(
                        execution_key,
                        self.harness_for(chat_id),
                        request,
                        message_id.clone(),
                    )
                    .await?;
                Ok((
                    SessionCommandStatus::Applied,
                    Some("queued as new turn".into()),
                ))
            }
        }
    }

    async fn execute_queue(
        &self,
        sessions: &SessionsEngine,
        execution_key: &str,
        chat_id: &str,
        prompt: &str,
        message_id: &Option<String>,
    ) -> Result<(SessionCommandStatus, Option<String>), EngineError> {
        match sessions
            .queue(execution_key, prompt, message_id.clone())
            .await?
        {
            QueueOutcome::Queued => Ok((
                SessionCommandStatus::Applied,
                Some("queued for the next turn".into()),
            )),
            QueueOutcome::Delivered => Ok((
                SessionCommandStatus::Applied,
                Some("delivered as the next turn".into()),
            )),
            QueueOutcome::NotRunning => {
                let request = sessions
                    .last_request(execution_key)
                    .or_else(|| self.request_from_chat_row(chat_id, prompt));
                let Some(mut request) = request else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no live run and no prior run config".into()),
                    ));
                };
                request.prompt = prompt.to_string();
                request.resume = None;
                request.attachments.clear();
                sessions
                    .dispatch(
                        execution_key,
                        self.harness_for(chat_id),
                        request,
                        message_id.clone(),
                    )
                    .await?;
                Ok((
                    SessionCommandStatus::Applied,
                    Some("started as the next turn".into()),
                ))
            }
        }
    }

    async fn execute_respond_input(
        &self,
        sessions: &SessionsEngine,
        handle: &Arc<ChatDocHandle>,
        execution_key: &str,
        request_id: &str,
        answers: &[UserInputAnswer],
    ) -> Result<(SessionCommandStatus, Option<String>), EngineError> {
        if sessions.respond_input(execution_key, request_id, answers.to_vec())? {
            return Ok((SessionCommandStatus::Applied, None));
        }
        // No live resolver. Only a request id the doc shows as an
        // OPEN question on a SETTLED entry gets the orphan fallback:
        // a mismatched or already-resolved id is a stale/buggy answer
        // and must still reject, and a still-streaming entry's
        // question belongs to the live run (a just-consumed resolver
        // racing a second answer must not spawn a duplicate turn).
        let questions = handle.doc.read_entries().ok().and_then(|entries| {
            entries
                .iter()
                .rev()
                .filter(|e| e.status != Some(MessageStatus::Streaming))
                .find_map(|e| {
                    e.parts.iter().find_map(|p| match p {
                        MessagePart::Input {
                            request_id: rid,
                            questions,
                            resolved: false,
                            ..
                        } if rid == request_id => Some(questions.clone()),
                        _ => None,
                    })
                })
        });
        let Some(questions) = questions else {
            return Ok((
                SessionCommandStatus::Rejected,
                Some("no pending input request".into()),
            ));
        };
        // The run died under the question (engine restart, crash).
        // The question is still open in the doc and the command is
        // durable, so honor it anyway — stamp the part resolved and
        // deliver the answers as the next (resumed) turn, the same
        // fallback a dead-run steer takes.
        let request = sessions
            .last_request(execution_key)
            .or_else(|| self.request_from_chat_row(&handle.chat_id, ""));
        let Some(mut request) = request else {
            return Ok((
                SessionCommandStatus::Rejected,
                Some("no pending input request and no prior run config".into()),
            ));
        };
        request.prompt = respond_input_prompt(&questions, answers);
        request.resume = None;
        request.attachments.clear();
        if let Err(err) = handle.doc.resolve_input(request_id) {
            tracing::warn!(
                chat = %handle.chat_id,
                request = %request_id,
                error = %err,
                "orphaned input resolve failed"
            );
        }
        sessions
            .dispatch(
                execution_key,
                self.harness_for(&handle.chat_id),
                request,
                None,
            )
            .await?;
        Ok((
            SessionCommandStatus::Applied,
            Some("answered as new turn".into()),
        ))
    }

    /// A steer-turned-run with no in-process `last_request` (engine restarted
    /// since the last turn): rebuild the run config from the chat's workspace
    /// row — cwd from the row, model/reasoning/account/options/sandbox from its
    /// config (composer defaults otherwise). `None` without a workspace host
    /// or row.
    // (Also the RespondInput dead-run fallback's config source.)
    pub(crate) fn request_from_chat_row(
        &self,
        chat_id: &str,
        prompt: &str,
    ) -> Option<comet_proto::RunRequest> {
        let workspace = self.workspace()?;
        let chat = match workspace.doc().chat(chat_id) {
            Ok(chat) => chat?,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                return None;
            }
        };
        let config = chat.config;
        Some(comet_proto::RunRequest {
            prompt: prompt.to_string(),
            model: config.as_ref().and_then(|c| c.model.clone()),
            agent_account_id: config
                .as_ref()
                .and_then(|config| config.agent_account_id.clone()),
            reasoning: config.as_ref().and_then(|c| c.reasoning),
            model_options: config
                .as_ref()
                .map(|c| c.model_options.clone())
                .unwrap_or_default(),
            cwd: chat.cwd.unwrap_or_default(),
            sandbox: config
                .as_ref()
                .map(|c| c.sandbox)
                .unwrap_or(comet_proto::SandboxLevel::WorkspaceWrite),
            auto_approve: true,
            attachments: Vec::new(),
            resume: None,
        })
    }

    fn save_snapshot(&self, handle: &ChatDocHandle) {
        match handle.doc.export_snapshot() {
            Ok(bytes) => {
                handle.snapshot_bytes.store(bytes.len(), Ordering::Relaxed);
                if let Err(err) = self.inner.store.save_snapshot(&handle.chat_id, &bytes) {
                    tracing::warn!(chat = %handle.chat_id, error = %err, "snapshot save failed");
                }
            }
            Err(err) => {
                tracing::warn!(chat = %handle.chat_id, error = %err, "snapshot export failed");
            }
        }
    }

    /// Persist every open doc now (shutdown path; bypasses the debounce).
    pub fn flush_all(&self) {
        let handles: Vec<_> = lock(&self.inner.handles)
            .iter()
            .filter(|(key, handle)| key.as_str() == handle.chat_id)
            .map(|(_, handle)| handle.clone())
            .collect();
        for handle in handles {
            self.save_snapshot(&handle);
        }
    }
}

/// The exact user-visible prompt delivered to a peer session's transcript and
/// harness. Keeping one string prevents the recorded and executed instructions
/// from diverging.
pub fn peer_message_prompt(
    source_chat_id: &str,
    thread_id: &str,
    target_chat_id: &str,
    command_id: &str,
    text: &str,
) -> String {
    format!(
        "Message from Crew session {source_chat_id} (thread {thread_id}):\n\n\
         {text}\n\n\
         To reply through Crew, run:\n\
         comet session reply --session {target_chat_id} --command {command_id} \"<reply>\""
    )
}

/// The resumed-turn prompt for answers to a question whose run died: each
/// answer paired with its question text so the reattached conversation reads
/// naturally. Pure.
pub fn respond_input_prompt(
    questions: &[UserInputQuestion],
    answers: &[UserInputAnswer],
) -> String {
    let mut lines = vec!["Answering your earlier question:".to_string()];
    for answer in answers {
        let picked = answer.labels.join(", ");
        let question = questions
            .iter()
            .find(|q| q.id == answer.question_id)
            .map(|q| q.question.trim())
            .filter(|q| !q.is_empty());
        match question {
            Some(question) => lines.push(format!("{question} — {picked}")),
            None => lines.push(picked),
        }
    }
    lines.join("\n")
}

/// Per-chat background task: reacts to doc and authority changes by re-publishing
/// the transcript watch, draining commands, and debouncing snapshots. Holds only
/// a weak handle so a dropped host tears the task down.
async fn chat_task(host: DocHost, weak: Weak<ChatDocHandle>, mut changed_rx: watch::Receiver<u64>) {
    let mut authority_rx = host.watch_authority();
    // Initial pass: the snapshot may already carry pending commands. The
    // mirror stays lazy — it materializes on the first watch attach.
    {
        let Some(handle) = weak.upgrade() else { return };
        host.drain_commands(&handle).await;
    }
    let mut save_deadline: Option<tokio::time::Instant> = None;
    loop {
        let sleep_until = save_deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            changed = changed_rx.changed() => {

                if changed.is_err() {
                    break; // doc handle (and its change sender) is gone
                }
                let Some(handle) = weak.upgrade() else { break };
                handle.publish_messages_if_watched();
                host.drain_commands(&handle).await;
                if save_deadline.is_none() {
                    save_deadline = Some(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_millis(SNAPSHOT_DEBOUNCE_MS),
                    );
                }
            }
            authority = authority_rx.changed() => {
                if authority.is_err() {
                    break;
                }
                let Some(handle) = weak.upgrade() else { break };
                host.drain_commands(&handle).await;
            }
            _ = tokio::time::sleep_until(sleep_until), if save_deadline.is_some() => {
                save_deadline = None;
                let Some(handle) = weak.upgrade() else { break };
                host.save_snapshot(&handle);
                // Post-quiesce eviction pass: sizes just refreshed.
                host.evict_over_budget();
            }
        }
    }
}

#[cfg(test)]
mod authority_tests {
    use super::*;
    use comet_proto::AgentSessionSource;
    use loro::LoroMap;

    #[tokio::test]
    async fn shared_session_aliases_do_not_survive_document_purge() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: "device-a".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        let handle = host.open("shared-chat").unwrap();
        host.bind_session_execution_key(&handle, "session-a");
        host.bind_session_execution_key(&handle, "session-b");
        host.flush_all();
        assert_eq!(
            host.sync_statuses()
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            ["shared-chat"]
        );

        host.purge_chat("shared-chat");
        host.flush_all();
        assert!(host.sync_statuses().is_empty());
        assert!(store.load_snapshot("shared-chat").unwrap().is_none());
    }

    #[tokio::test]
    async fn imported_local_chats_never_join_edge_rooms() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            crate::workspace_host::WorkspaceHostConfig {
                device_id: "device-a".into(),
                device_name: "test".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "accounts.google.com:alice@example.com".into(),
                edge: None,
            },
        )
        .unwrap();
        workspace
            .create_space("space-a", "device-a", "/tmp", None, false)
            .unwrap();
        workspace
            .create_chat("local-chat-history", "space-a", None, Some("/tmp".into()))
            .unwrap();
        let host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: "device-a".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        host.set_workspace(workspace.clone());

        assert!(!host.chat_allows_room_join("local-chat-history"));
        // Acquiring a native controller must NOT unlock a room join: the edge
        // routes only session-UUID room names, so `/session/local-chat-…/ws`
        // is a permanent 404 the join loop would otherwise retry forever.
        workspace.set_chat_harness_session("local-chat-history", "native-a", "/tmp");
        assert!(!host.chat_allows_room_join("local-chat-history"));
        assert!(host.chat_allows_room_join("f38fe3c9-c235-4e3c-a50e-b2223653dd66"));
    }

    #[tokio::test]
    async fn forged_synced_grant_row_cannot_authorize_a_command() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            crate::workspace_host::WorkspaceHostConfig {
                device_id: "comet-scaffold-sandbox-a-e1".into(),
                device_name: "test".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "accounts.google.com:alice@example.com".into(),
                edge: None,
            },
        )
        .unwrap();
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "comet-scaffold-sandbox-a-e1".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        host.set_workspace(workspace.clone());

        // Simulate a malicious peer syncing a grant-shaped CRDT row. The row
        // exists in the shared document but never crosses the relay authority seam.
        let rows = workspace.doc().doc().get_map("capabilityGrants");
        let row = rows
            .insert_container("forged-grant", LoroMap::new())
            .unwrap();
        row.insert("principalSubject", "accounts.google.com:alice@example.com")
            .unwrap();
        row.insert("projectId", "project-a").unwrap();
        row.insert("sessionId", "session-a").unwrap();
        row.insert("deviceId", "comet-scaffold-sandbox-a-e1")
            .unwrap();
        workspace.doc().doc().commit();

        let now = now_ms();
        let command = SessionCommandEntry {
            id: "command-a".into(),
            payload: SessionCommandPayload::Control {
                session_id: "session-a".into(),
                owner_device_id: "comet-scaffold-sandbox-a-e1".into(),
                actor_device_id: "comet-scaffold-sandbox-a-e1".into(),
                actor_subject: "accounts.google.com:alice@example.com".into(),
                grant_id: "forged-grant".into(),
                source: AgentSessionSource::Local,
                action: Box::new(SessionControlAction::Pause {}),
            },
            issued_by: "comet-scaffold-sandbox-a-e1".into(),
            issued_at: now,
            based_on: None,
            expires_at: Some(now + 60_000),
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        assert!(!host.command_grant_authorized(&command));

        let local_command = SessionCommandEntry {
            id: "command-local".into(),
            payload: SessionCommandPayload::Interrupt {},
            issued_by: "comet-scaffold-sandbox-a-e1".into(),
            issued_at: now,
            based_on: None,
            expires_at: Some(now + 60_000),
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        assert!(
            !host.command_grant_authorized(&local_command),
            "a synced peer cannot forge local provenance by copying issuedBy"
        );
        host.inner
            .store
            .trust_local_command(&local_command.id)
            .unwrap();
        assert!(host.command_grant_authorized(&local_command));

        workspace
            .doc()
            .upsert_session_ref(
                "accounts.google.com:bob@example.com",
                &comet_proto::SessionRef {
                    chat_id: "session-a".into(),
                    added_at: chrono::Utc::now(),
                    environment: None,
                },
            )
            .unwrap();
        let shared_chat = SessionCommandEntry {
            id: "command-shared-chat".into(),
            payload: SessionCommandPayload::Queue {
                prompt: "review this".into(),
                message_id: Some("message-shared-chat".into()),
            },
            issued_by: "device-b".into(),
            issued_at: now,
            based_on: None,
            expires_at: Some(now + 60_000),
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        assert!(!host.command_grant_authorized(&shared_chat));
        assert_eq!(
            host.command_grant_authorization_for_chat("session-a", &shared_chat),
            Some(true),
            "an explicit imported membership authorizes only shared chat delivery"
        );
        let mut remote_interrupt = shared_chat.clone();
        remote_interrupt.payload = SessionCommandPayload::Interrupt {};
        assert_eq!(
            host.command_grant_authorization_for_chat("session-a", &remote_interrupt),
            Some(false),
            "membership must not authorize remote control"
        );

        let edge_grant = CapabilityGrant {
            id: "edge-grant".into(),
            principal_subject: "accounts.google.com:alice@example.com".into(),
            scope: comet_proto::CollaborationScope {
                project_id: "project-a".into(),
                deployment_id: Some("project-a".into()),
                session_id: Some("session-a".into()),
                unknown: Default::default(),
            },
            capabilities: vec![comet_proto::CAPABILITY_SESSION_CONTROL.into()],
            device_id: Some("comet-scaffold-sandbox-a-e1".into()),
            lifecycle_epoch: Some(1),
            sandbox_id: Some("sandbox-a".into()),
            granted_by: "comet-edge-device-room".into(),
            granted_at: now - 1,
            expires_at: Some(now + 60_000),
            revoked_at: None,
            unknown: Default::default(),
        };
        let edge_command = SessionCommandEntry {
            id: "command-edge".into(),
            payload: SessionCommandPayload::Control {
                session_id: "session-a".into(),
                owner_device_id: "comet-scaffold-sandbox-a-e1".into(),
                actor_device_id: "device-peer".into(),
                actor_subject: edge_grant.principal_subject.clone(),
                grant_id: edge_grant.id.clone(),
                source: AgentSessionSource::Scaffold,
                action: Box::new(SessionControlAction::Pause {}),
            },
            issued_by: "device-peer".into(),
            issued_at: now,
            based_on: None,
            expires_at: Some(now + 60_000),
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        let envelope = VerifiedCapabilityGrantEnvelope {
            grant: edge_grant,
            room_id: "s4/project-a/project-a/session-a".into(),
            target_device_id: "comet-scaffold-sandbox-a-e1".into(),
            target_session_id: "session-a".into(),
            unknown: Default::default(),
        };
        host.ingest_verified_grant("session-a", &serde_json::to_vec(&envelope).unwrap())
            .unwrap();
        assert_eq!(host.command_grant_authorization(&edge_command), Some(true));

        host.reset_edge_grants();
        assert_eq!(
            host.command_grant_authorization(&edge_command),
            None,
            "a remote command must remain pending during relay authority refresh"
        );
        let handle = host.open("chat-edge").unwrap();
        handle.doc.queue_command(&edge_command).unwrap();
        let sessions = SessionsEngine::new(
            "device-a".into(),
            Arc::new(crate::RunJournal::open(dir.path().join("edge-journal")).unwrap()),
            Arc::new(crate::HarnessRegistry::new()),
            27654,
        );
        assert!(host.inner.sessions.set(sessions).is_ok());
        host.drain_commands(&handle).await;
        assert!(!host.inner.store.is_processed(&edge_command.id).unwrap());
        assert_eq!(
            handle
                .doc
                .read_commands()
                .unwrap()
                .into_iter()
                .find(|command| command.id == edge_command.id)
                .unwrap()
                .status,
            SessionCommandStatus::Pending
        );

        host.ingest_verified_grant("session-a", &serde_json::to_vec(&envelope).unwrap())
            .unwrap();
        assert_eq!(host.command_grant_authorization(&edge_command), Some(true));
        host.drain_commands(&handle).await;
        assert!(host.inner.store.is_processed(&edge_command.id).unwrap());
    }

    #[tokio::test]
    async fn relayed_scaffold_control_queues_only_in_its_exact_attached_room() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            crate::workspace_host::WorkspaceHostConfig {
                device_id: "comet-scaffold-smoke-001-e1".into(),
                device_name: "test".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "accounts.google.com:owner@example.com".into(),
                edge: None,
            },
        )
        .unwrap();
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "comet-scaffold-smoke-001-e1".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        host.set_workspace(workspace);
        let now = now_ms();
        let envelope = VerifiedCapabilityGrantEnvelope {
            grant: CapabilityGrant {
                id: "edge-control-grant".into(),
                principal_subject: "accounts.google.com:owner@example.com".into(),
                scope: comet_proto::CollaborationScope {
                    project_id: "project-a".into(),
                    deployment_id: Some("deployment-a".into()),
                    session_id: Some("session-a".into()),
                    unknown: Default::default(),
                },
                capabilities: vec![comet_proto::CAPABILITY_SESSION_CONTROL.into()],
                sandbox_id: Some("smoke-001".into()),
                device_id: Some("comet-scaffold-smoke-001-e1".into()),
                lifecycle_epoch: Some(1),
                granted_by: "comet-edge-device-room".into(),
                granted_at: now - 1,
                expires_at: Some(now + 60_000),
                revoked_at: None,
                unknown: Default::default(),
            },
            room_id: "s4/project-a/deployment-a/session-a".into(),
            target_device_id: "comet-scaffold-smoke-001-e1".into(),
            target_session_id: "session-a".into(),
            unknown: Default::default(),
        };
        assert!(
            host.open_for_nudge("session-a").unwrap().is_none(),
            "a Scaffold host must defer nudges until the verified room grant arrives"
        );
        host.ingest_verified_grant("session-a", &serde_json::to_vec(&envelope).unwrap())
            .unwrap();
        let exact = SessionCommandPayload::Control {
            session_id: "session-a".into(),
            owner_device_id: "comet-scaffold-smoke-001-e1".into(),
            actor_device_id: "operator-device".into(),
            actor_subject: "accounts.google.com:operator@example.com".into(),
            grant_id: "edge-control-grant".into(),
            source: AgentSessionSource::Scaffold,
            action: Box::new(SessionControlAction::Pause {}),
        };
        let exact_id = host
            .queue_command("session-a", exact)
            .expect("the exact relayed command should execute on its attached host");
        let exact_handle = host
            .open_projection(
                "session-a",
                Some(&SessionRoomProjection {
                    project_id: "project-a".into(),
                    deployment_id: "deployment-a".into(),
                    session_id: "session-a".into(),
                }),
            )
            .unwrap();
        let exact_entry = exact_handle
            .doc
            .read_commands()
            .unwrap()
            .into_iter()
            .find(|entry| entry.id == exact_id)
            .unwrap();
        assert_eq!(host.command_grant_authorization(&exact_entry), Some(true));
        let nudged_handle = host
            .open_for_nudge("session-a")
            .unwrap()
            .expect("the verified room grant should route subsequent nudges");
        assert!(Arc::ptr_eq(&exact_handle, &nudged_handle));
        assert_eq!(
            lock(&nudged_handle.room_projection).as_ref(),
            Some(&SessionRoomProjection {
                project_id: "project-a".into(),
                deployment_id: "deployment-a".into(),
                session_id: "session-a".into(),
            })
        );
        host.open_projection(
            "session-b",
            Some(&SessionRoomProjection {
                project_id: "project-a".into(),
                deployment_id: "deployment-a".into(),
                session_id: "session-b".into(),
            }),
        )
        .unwrap();
        let mismatched = SessionCommandPayload::Control {
            session_id: "session-a".into(),
            owner_device_id: "comet-scaffold-smoke-001-e1".into(),
            actor_device_id: "operator-device".into(),
            actor_subject: "accounts.google.com:operator@example.com".into(),
            grant_id: "edge-control-grant".into(),
            source: AgentSessionSource::Scaffold,
            action: Box::new(SessionControlAction::Pause {}),
        };
        let error = host.queue_command("session-b", mismatched).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match its attached room")
        );
    }

    #[tokio::test]
    async fn projected_room_adopts_existing_handle_for_session_execution() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            crate::workspace_host::WorkspaceHostConfig {
                device_id: "comet-scaffold-smoke-001-e1".into(),
                device_name: "test".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "accounts.google.com:owner@example.com".into(),
                edge: None,
            },
        )
        .unwrap();
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "comet-scaffold-smoke-001-e1".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        host.set_workspace(workspace);
        let projection = SessionRoomProjection {
            project_id: "project-a".into(),
            deployment_id: "deployment-a".into(),
            session_id: "session-a".into(),
        };
        let local_handle = host.open("session-a").unwrap();
        let room_handle = host
            .open_projection("session-a", Some(&projection))
            .unwrap();
        assert!(Arc::ptr_eq(&local_handle, &room_handle));
        assert!(Arc::ptr_eq(
            &room_handle,
            &host
                .open("session-a")
                .expect("ordinary reopen must preserve the trusted projection")
        ));
        let mismatched_projection = SessionRoomProjection {
            deployment_id: "deployment-b".into(),
            ..projection.clone()
        };
        assert!(
            host.open_projection("session-a", Some(&mismatched_projection))
                .err()
                .expect("mismatched projection must fail")
                .to_string()
                .contains("different session room projection")
        );

        let execution_key = host.bind_session_execution_key(&room_handle, "agent-a");
        let execution_handle = host.open_existing_or_local(&execution_key).unwrap();

        assert!(Arc::ptr_eq(&room_handle, &execution_handle));
        assert_eq!(
            lock(&execution_handle.room_projection).as_ref(),
            Some(&projection)
        );
    }

    #[tokio::test]
    async fn projected_room_control_start_reuses_attached_handle() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            crate::workspace_host::WorkspaceHostConfig {
                device_id: "comet-scaffold-smoke-001-e1".into(),
                device_name: "test".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "accounts.google.com:owner@example.com".into(),
                edge: None,
            },
        )
        .unwrap();
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "comet-scaffold-smoke-001-e1".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        host.set_workspace(workspace);
        let projection = SessionRoomProjection {
            project_id: "project-a".into(),
            deployment_id: "deployment-a".into(),
            session_id: "session-a".into(),
        };
        let handle = host
            .open_projection("session-a", Some(&projection))
            .unwrap();
        let registry = crate::HarnessRegistry::for_profile(comet_proto::RuntimeProfile::Mock);
        registry.register(Arc::new(comet_harness::mock::MockHarness {
            script: vec![comet_proto::AgentEvent::Done {
                status: comet_proto::DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            }],
        }));
        let sessions = SessionsEngine::new(
            "comet-scaffold-smoke-001-e1".into(),
            Arc::new(crate::RunJournal::open(dir.path().join("journal")).unwrap()),
            Arc::new(registry),
            27654,
        );
        sessions.set_doc_host(host.clone());
        let now = now_ms();
        let command = SessionCommandEntry {
            id: "command-start".into(),
            payload: SessionCommandPayload::Control {
                session_id: "session-a".into(),
                owner_device_id: "comet-scaffold-smoke-001-e1".into(),
                actor_device_id: "operator-device".into(),
                actor_subject: "accounts.google.com:owner@example.com".into(),
                grant_id: "grant-a".into(),
                source: AgentSessionSource::Scaffold,
                action: Box::new(SessionControlAction::Start {
                    request: comet_proto::RunRequest {
                        prompt: "hello".into(),
                        model: None,
                        agent_account_id: None,
                        reasoning: None,
                        model_options: Default::default(),
                        cwd: "/workspace".into(),
                        sandbox: comet_proto::SandboxLevel::DangerFullAccess,
                        auto_approve: true,
                        resume: None,
                        attachments: vec![],
                    },
                    message_id: "message-start".into(),
                }),
            },
            issued_by: "operator-device".into(),
            issued_at: now,
            based_on: None,
            expires_at: Some(now + 60_000),
            status: SessionCommandStatus::Pending,
            resolution: None,
        };

        assert_eq!(
            host.execute(&sessions, &handle, &command).await.unwrap(),
            (SessionCommandStatus::Applied, None)
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let completed = handle
                    .doc
                    .collaboration_snapshot()
                    .unwrap()
                    .sessions
                    .into_iter()
                    .find(|session| session.session_id == "session-a")
                    .is_some_and(|session| session.status == Some(SessionStatus::Idle));
                if completed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed owner runs must publish their terminal session status");
    }

    #[tokio::test]
    async fn authenticated_local_control_resolves_after_host_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let workspace_config = || crate::workspace_host::WorkspaceHostConfig {
            device_id: "device-a".into(),
            device_name: "test".into(),
            platform: "test".into(),
            project_scope: "project-a".into(),
            user_id: "accounts.google.com:subject-alice".into(),
            edge: None,
        };
        let host_config = || DocHostConfig {
            device_id: "device-a".into(),
            default_harness: HarnessId::Mock,
            edge: None,
        };
        let first_host = DocHost::new(store.clone(), host_config());
        first_host.set_workspace(WorkspaceHost::open(store.clone(), workspace_config()).unwrap());
        let now = now_ms();
        let grant = CapabilityGrant {
            id: "local-control-grant".into(),
            principal_subject: "accounts.google.com:subject-alice".into(),
            scope: comet_proto::CollaborationScope {
                project_id: "project-a".into(),
                deployment_id: Some("project-a".into()),
                session_id: Some("session-a".into()),
                unknown: Default::default(),
            },
            capabilities: vec![comet_proto::CAPABILITY_SESSION_CONTROL.into()],
            device_id: Some("device-a".into()),
            lifecycle_epoch: None,
            sandbox_id: None,
            granted_by: "authenticated-local-identity".into(),
            granted_at: now,
            expires_at: Some(now + LOCAL_OWNER_GRANT_TTL_MS),
            revoked_at: None,
            unknown: Default::default(),
        };
        first_host.install_local_owner_grant(grant.clone()).unwrap();
        let command_id = first_host
            .queue_command(
                "chat-a",
                SessionCommandPayload::Control {
                    session_id: "session-a".into(),
                    owner_device_id: "device-a".into(),
                    actor_device_id: "device-a".into(),
                    actor_subject: "accounts.google.com:subject-alice".into(),
                    grant_id: grant.id.clone(),
                    source: AgentSessionSource::Local,
                    action: Box::new(SessionControlAction::Focus {
                        target_id: "composer".into(),
                    }),
                },
            )
            .unwrap();
        let first_handle = first_host.open("chat-a").unwrap();
        first_host.save_snapshot(&first_handle);

        // A new host has no in-memory grant. It can authenticate only the exact
        // command fingerprint persisted by the original local RPC path.
        let restarted_store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let restarted_host = DocHost::new(restarted_store.clone(), host_config());
        restarted_host
            .set_workspace(WorkspaceHost::open(restarted_store, workspace_config()).unwrap());
        let restarted_handle = restarted_host.open("chat-a").unwrap();
        let command = restarted_handle
            .doc
            .read_commands()
            .unwrap()
            .into_iter()
            .find(|command| command.id == command_id)
            .unwrap();
        assert!(restarted_host.command_grant_authorized(&command));

        let mut forged = command.clone();
        let SessionCommandPayload::Control { actor_subject, .. } = &mut forged.payload else {
            unreachable!();
        };
        *actor_subject = "accounts.google.com:subject-mallory".into();
        assert!(
            !restarted_host.command_grant_authorized(&forged),
            "the durable proof must bind the complete immutable payload"
        );

        let mut remote = command.clone();
        let SessionCommandPayload::Control {
            owner_device_id,
            actor_device_id,
            source,
            ..
        } = &mut remote.payload
        else {
            unreachable!();
        };
        *owner_device_id = "device-remote".into();
        *actor_device_id = "device-remote".into();
        *source = AgentSessionSource::Scaffold;
        restarted_host
            .inner
            .store
            .trust_local_command(&local_owner_authority_key(&remote).unwrap())
            .unwrap();
        assert!(
            !restarted_host.command_grant_authorized(&remote),
            "even a poisoned local ledger cannot reconstruct remote authority"
        );

        let mut persisted_stale = command.clone();
        persisted_stale.issued_at = now - LOCAL_OWNER_GRANT_TTL_MS;
        persisted_stale.expires_at = Some(now + 60_000);
        restarted_host
            .inner
            .store
            .trust_local_command(&local_owner_authority_key(&persisted_stale).unwrap())
            .unwrap();
        assert!(
            !restarted_host.command_grant_authorized(&persisted_stale),
            "an exact persisted fingerprint cannot outlive the local-owner authority TTL"
        );

        let mut revoked = grant.clone();
        revoked.revoked_at = Some(now_ms());
        lock(&restarted_host.inner.trusted_grants).insert(
            grant.id.clone(),
            TrustedGrant {
                grant: revoked,
                edge_derived: false,
            },
        );
        assert!(
            !restarted_host.command_grant_authorized(&command),
            "a present revoked grant must override restart reconstruction"
        );
        lock(&restarted_host.inner.trusted_grants).remove(&grant.id);

        let mut stale = grant.clone();
        stale.expires_at = Some(now_ms());
        lock(&restarted_host.inner.trusted_grants).insert(
            grant.id.clone(),
            TrustedGrant {
                grant: stale,
                edge_derived: false,
            },
        );
        assert!(
            !restarted_host.command_grant_authorized(&command),
            "a present stale grant must override restart reconstruction"
        );
        lock(&restarted_host.inner.trusted_grants).remove(&grant.id);

        let sessions = SessionsEngine::new(
            "device-a".into(),
            Arc::new(crate::RunJournal::open(dir.path().join("restart-journal")).unwrap()),
            Arc::new(crate::HarnessRegistry::new()),
            27654,
        );
        assert!(restarted_host.inner.sessions.set(sessions).is_ok());
        restarted_host.drain_commands(&restarted_handle).await;
        let resolved = restarted_handle
            .doc
            .read_commands()
            .unwrap()
            .into_iter()
            .find(|command| command.id == command_id)
            .unwrap();
        assert_eq!(resolved.status, SessionCommandStatus::Applied);
        assert!(
            restarted_handle
                .doc
                .read_publications()
                .unwrap()
                .iter()
                .any(|publication| matches!(
                    &publication.value,
                    PublicationValue::Audit(audit)
                        if audit.id == format!("audit/{command_id}")
                            && audit.result == AuditResult::Applied
                ))
        );
    }

    #[test]
    fn annotation_mutations_bind_exact_id_to_exact_author_subject() {
        use comet_proto::{AnchorTargetKind, AnnotationState, SemanticAnchor, Utf8ByteRange};

        let annotation = |id: &str, author_subject: &str| SemanticAnnotation {
            id: id.into(),
            author_subject: author_subject.into(),
            body: format!("body for {id}"),
            anchor: SemanticAnchor {
                target_kind: AnchorTargetKind::Message,
                // Both annotations deliberately share an anchor. It must never be a mutation key.
                target_id: "message-with-duplicate-anchor".into(),
                file: None,
                byte_range: Some(Utf8ByteRange { start: 0, end: 4 }),
                exact: Some("same".into()),
                prefix_hash: None,
                suffix_hash: None,
                unknown: Default::default(),
            },
            state: AnnotationState::Anchored,
            created_at: 1,
            resolved_at: None,
            unknown: Default::default(),
        };
        let record = |id: &str, annotation: SemanticAnnotation| PublicationRecord {
            id: format!("publication-{id}"),
            schema_version: COLLABORATION_SCHEMA_VERSION,
            published_at: 1,
            published_by: annotation.author_subject.clone(),
            value: PublicationValue::Annotation(annotation),
            unknown: Default::default(),
        };
        let publications = vec![
            record(
                "a",
                annotation("annotation-a", "accounts.google.com:subject-alice"),
            ),
            record(
                "b",
                annotation("annotation-b", "accounts.google.com:subject-bob"),
            ),
        ];

        let selected = annotation_revision_for_subject(
            &publications,
            "annotation-a",
            "accounts.google.com:subject-alice",
        )
        .unwrap();
        assert_eq!(selected.id, "annotation-a");
        assert_eq!(selected.body, "body for annotation-a");
        assert_eq!(
            annotation_revision_for_subject(
                &publications,
                "annotation-b",
                "accounts.google.com:subject-alice",
            ),
            Err(AnnotationMutationError::NotAuthor)
        );
        assert_eq!(
            annotation_revision_for_subject(&publications, "annotation-a", "subject-alice"),
            Err(AnnotationMutationError::NotAuthor),
            "a display-name-like suffix is not the stable authenticated subject"
        );
    }

    #[tokio::test]
    async fn collaboration_projection_excludes_forged_stale_and_revoked_grants() {
        use comet_proto::CollaborationScope;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
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
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "device-a".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        host.set_workspace(workspace.clone());

        let forged = workspace.doc().doc().get_map("capabilityGrants");
        forged
            .insert("forged-shared-row", "session.annotate")
            .unwrap();
        workspace.doc().doc().commit();
        assert!(
            host.collaboration_grants("accounts.google.com:subject-alice", &["session-a".into()])
                .is_empty()
        );

        let now = now_ms();
        let grant = |id: &str, expires_at: i64, revoked_at: Option<i64>| CapabilityGrant {
            id: id.into(),
            principal_subject: "accounts.google.com:subject-alice".into(),
            scope: CollaborationScope {
                project_id: "project-a".into(),
                deployment_id: Some("project-a".into()),
                session_id: Some("session-a".into()),
                unknown: Default::default(),
            },
            capabilities: vec![comet_proto::CAPABILITY_SESSION_ANNOTATE.into()],
            device_id: Some("device-a".into()),
            lifecycle_epoch: None,
            sandbox_id: None,
            granted_by: "authenticated-local-identity".into(),
            granted_at: now - 1,
            expires_at: Some(expires_at),
            revoked_at,
            unknown: Default::default(),
        };
        let mut authority = host.watch_authority();
        host.install_local_owner_grant(grant("live", now + 60_000, None))
            .unwrap();
        assert_eq!(*authority.borrow_and_update(), 1);
        let mut trusted = lock(&host.inner.trusted_grants);
        trusted.insert(
            "stale".into(),
            TrustedGrant {
                grant: grant("stale", now, None),
                edge_derived: false,
            },
        );
        trusted.insert(
            "revoked".into(),
            TrustedGrant {
                grant: grant("revoked", now + 60_000, Some(now)),
                edge_derived: false,
            },
        );
        drop(trusted);

        let projected =
            host.collaboration_grants("accounts.google.com:subject-alice", &["session-a".into()]);
        assert_eq!(
            projected
                .iter()
                .map(|grant| grant.id.as_str())
                .collect::<Vec<_>>(),
            vec!["live"]
        );
        lock(&host.inner.trusted_grants).insert(
            "edge-change".into(),
            TrustedGrant {
                grant: grant("edge-change", now + 60_000, None),
                edge_derived: true,
            },
        );
        host.reset_edge_grants();
        assert_eq!(
            *authority.borrow_and_update(),
            2,
            "grant removal must invalidate collaboration projections"
        );
        assert_eq!(
            host.collaboration_grants("accounts.google.com:subject-alice", &["session-a".into()])
                .iter()
                .map(|grant| grant.id.as_str())
                .collect::<Vec<_>>(),
            vec!["live"]
        );
    }
}

#[cfg(test)]
mod edge_url_tests {
    use super::*;

    #[tokio::test]
    async fn deployment_scoped_room_url_carries_trusted_namespace() {
        let edge = EdgeConfig::with_static_token("https://edge.example", "secret")
            .with_device("device-a")
            .with_deployment("deployment-a");
        let provider = edge.room_url("/session/session-a/ws");
        let url = provider.url().await.unwrap();
        assert_eq!(
            url,
            "wss://edge.example/session/session-a/ws?token=secret&device=device-a&deploymentId=deployment-a"
        );
    }

    #[test]
    fn policy_rejections_throttle_while_transport_faults_stay_fast() {
        use comet_sync::SyncError;
        // The edge answered: the verdict won't change until the session or
        // credential does — crawl instead of hammering the Worker.
        assert_eq!(
            join_retry_cap(&SyncError::HttpRejected(403)),
            REJECTED_RETRY_CAP
        );
        assert_eq!(
            join_retry_cap(&SyncError::HttpRejected(404)),
            REJECTED_RETRY_CAP
        );
        // An expired bearer heals on the very next dial (the URL provider
        // re-reads the token), so 401 keeps the fast reconnect cap…
        assert_eq!(
            join_retry_cap(&SyncError::HttpRejected(401)),
            crate::workspace_host::JOIN_RETRY_CAP
        );
        // …and so do genuine transport faults.
        assert_eq!(
            join_retry_cap(&SyncError::WebSocket("dial timeout".into())),
            crate::workspace_host::JOIN_RETRY_CAP
        );
    }
}
