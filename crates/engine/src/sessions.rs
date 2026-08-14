//! SessionsEngine — per-chat agent runs: dispatch, steering, interrupts, input bridging,
//! journal + broadcast fan-out, and 120ms coalesced doc streaming.
//!
//! Pragmatic port of comet's `sessions.ts` (spec: feature-inventory §3.2):
//! - every `AgentEvent` is (a) appended to the on-disk run journal, (b) broadcast to
//!   in-process subscribers, (c) folded via `fold_event_into_parts` and diffed into the
//!   chat's `SessionDoc` through `SegmentWriter` on a coalesced `STREAM_COMMIT_MS` timer;
//! - the user message entry is pushed to the doc immediately on dispatch (id = the
//!   command's client-minted message id, so optimistic echoes never flicker);
//! - a `Steered` event splits the assistant entry at the exact boundary;
//! - recovery (interrupt or a stale journal at boot) stamps the streaming entry `aborted`.
//!
//! Scope notes: sessions are keyed by chat id (one live run per chat). Comet's pulse
//! loop is ported as the 15s liveness heartbeat in `drive_run`; its stall watchdog is
//! deliberately NOT ported (rejected in review — agents may legitimately wait on
//! something for far longer than any timeout, and a live child IS the working signal).
//! Every dying path must instead carry its own visible error (child crash with stderr,
//! spawn failure, stream error, engine-restart recovery).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use chrono::Utc;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot, watch};

use comet_doc::{
    DocError, MessagePart, MessageRole, MessageStatus, STREAM_COMMIT_MS, SegmentWriter, SessionDoc,
    fold_event_into_parts, sanitize_tool_call,
};
use comet_harness::{
    CancellationToken, Harness, HarnessError, RunContext, RunControls, SteerMessage,
};
use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, Session, SessionStatus, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};

use crate::doc_host::{ChatDocHandle, DocHost};
use crate::registry::HarnessRegistry;
use crate::run_journal::RunJournal;
use crate::{EngineError, new_id, now_ms};

/// [`SessionsEngine::tool_call_detail`]'s reduction of one tool id's journal
/// events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCallDetail {
    /// Full multiline input (pre-sanitize — journals keep what the doc strips).
    pub input: Option<String>,
    /// Harness-captured output, when the adapter reported one.
    pub output: Option<String>,
    pub is_error: bool,
    /// True once a ToolResult landed.
    pub resolved: bool,
}

/// One journaled event: the durable seq plus the event, as broadcast to subscribers.
#[derive(Debug, Clone)]
pub struct JournaledEvent {
    pub seq: u64,
    pub event: AgentEvent,
}

/// Outcome of a steer attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerOutcome {
    /// Accepted into the live run's mailbox; carries the stamped user-message
    /// delivery state — `Steered` (altered the active turn), `Queued` (waits
    /// for the turn boundary), or `Complete` (parked run, delivered as the
    /// next turn immediately).
    Accepted(MessageStatus),
    /// No live steerable run — the caller should dispatch the prompt as a new turn.
    NotSteerable,
}

/// Outcome of an explicit next-turn queue request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueOutcome {
    /// Held by the engine until the active turn finishes.
    Queued,
    /// The persistent harness was already parked, so the next turn started now.
    Delivered,
    /// No run exists; the caller should dispatch the prompt as a new turn.
    NotRunning,
}

type PendingInputs = Arc<Mutex<HashMap<String, oneshot::Sender<Vec<UserInputAnswer>>>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerReply {
    pub command_id: String,
    pub text: String,
    pub source_chat_id: String,
}

struct LivePeerWaiter {
    registration_id: String,
    sender: oneshot::Sender<PeerReply>,
}

pub struct PeerWaitClaim {
    sender: oneshot::Sender<PeerReply>,
}

impl PeerWaitClaim {
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    pub fn deliver(self, reply: PeerReply) -> bool {
        self.sender.send(reply).is_ok()
    }
}

/// One race-free subscription to a source-session peer thread. Dropping it
/// unregisters only this generation, so a timed-out waiter cannot remove a
/// later registration for the same thread.
pub struct PeerWaitRegistration {
    inner: Weak<Inner>,
    key: (String, String),
    registration_id: String,
    receiver: Option<oneshot::Receiver<PeerReply>>,
}

impl Drop for PeerWaitRegistration {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut waiters = lock(&inner.peer_waiters);
        if waiters
            .get(&self.key)
            .is_some_and(|waiter| waiter.registration_id == self.registration_id)
        {
            waiters.remove(&self.key);
        }
    }
}

/// A harness-native session id plus the cwd it was created under. Harness
/// session stores are cwd-scoped (claude keys conversations by project
/// directory — comet sessions.ts:563 "harness session stores are keyed by
/// cwd"), so resume is only injected for runs launched from the same cwd.
#[derive(Debug, Clone)]
struct HarnessSessionRef {
    session_id: String,
    cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunAuthIdentity {
    Unattached,
    SignedOut,
    SignedIn {
        owner_subject: String,
        project_scope: String,
    },
}

impl From<&crate::AuthState> for RunAuthIdentity {
    fn from(state: &crate::AuthState) -> Self {
        match state {
            crate::AuthState::SignedOut => Self::SignedOut,
            crate::AuthState::SignedIn {
                user,
                project_scope,
            } => Self::SignedIn {
                owner_subject: user.id.clone(),
                project_scope: project_scope.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunRoute {
    harness: HarnessId,
    model: Option<String>,
    agent_account_id: Option<String>,
    auth_identity: RunAuthIdentity,
}

impl RunRoute {
    fn new(harness: HarnessId, request: &RunRequest, auth_identity: RunAuthIdentity) -> Self {
        Self {
            harness,
            model: request.model.clone(),
            agent_account_id: request.agent_account_id.clone(),
            auth_identity,
        }
    }
}

#[derive(Debug)]
struct DispatchPreparation {
    id: String,
    route: RunRoute,
    cancel: CancellationToken,
}

struct DispatchPreparationGuard {
    inner: Weak<Inner>,
    chat_id: String,
    id: String,
    cancel: CancellationToken,
}

impl Drop for DispatchPreparationGuard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut preparations = lock(&inner.preparations);
        if preparations
            .get(&self.chat_id)
            .is_some_and(|preparation| preparation.id == self.id)
        {
            preparations.remove(&self.chat_id);
        }
    }
}

struct RunHandle {
    run_id: String,
    user_message_id: String,
    route: RunRoute,
    steerable: bool,
    steering_mode: SteeringMode,
    steer_tx: mpsc::Sender<SteerMessage>,
    /// True while the harness is producing the current turn. Updated under the
    /// same run-map lock used to enqueue followups, closing the Done/enqueue race.
    turn_active: bool,
    /// Explicit next-turn followups. Unlike the harness steering mailbox, this
    /// queue is not visible to step-boundary harnesses until the current Done.
    queued_followups: VecDeque<SteerMessage>,
    /// Turn-boundary steers accepted into a live turn's mailbox but not yet
    /// handed to the harness as a new prompt. Only steers that found an active
    /// turn are tracked; the matching `Steered` event acknowledges them FIFO.
    pending_turn_boundary_steers: VecDeque<SteerMessage>,
    /// Harness-level cancellation (protocol interrupt + child teardown).
    interrupt_token: CancellationToken,
    /// Engine-level cancel: arms the run task's grace deadline so a harness that
    /// ignores its token can never strand the run.
    cancel: watch::Sender<bool>,
    engine_tx: mpsc::UnboundedSender<AgentEvent>,
    pending_inputs: PendingInputs,
}

struct RouteRestartState {
    lifecycle_epoch: u64,
    restarting_from_run_id: String,
    restart_pending: bool,
}

impl RunHandle {
    /// Delivery state for a prompt just accepted into this run's mailbox.
    ///
    /// A parked persistent session (`!was_turn_active`) is already at the
    /// next-turn boundary: the prompt starts that turn immediately as plain
    /// delivered input — nothing is being steered. Mid-turn, a step-boundary
    /// harness alters the active turn now (`Steered`); a turn-boundary harness
    /// has only accepted a future prompt (`Queued`) until it acknowledges the
    /// pickup with a `Steered` event.
    fn mailbox_message_status(
        &mut self,
        was_turn_active: bool,
        message: SteerMessage,
    ) -> MessageStatus {
        if !was_turn_active {
            return MessageStatus::Complete;
        }
        match self.steering_mode {
            SteeringMode::StepBoundary => MessageStatus::Steered,
            SteeringMode::TurnBoundary => {
                self.pending_turn_boundary_steers.push_back(message);
                MessageStatus::Queued
            }
        }
    }
}

struct Inner {
    device_id: String,
    journal: Arc<RunJournal>,
    registry: Arc<HarnessRegistry>,
    /// Loopback port advertised to harness children for `comet session` RPC.
    ipc_port: u16,
    doc_host: OnceLock<DocHost>,
    /// chat_id → live run.
    runs: Mutex<HashMap<String, RunHandle>>,
    /// One automatic route-lifecycle restart may remain pending until the
    /// replacement run proves inference progress.
    route_restarts: Mutex<HashMap<String, RouteRestartState>>,
    /// Per-chat dispatch serialization. The weak values disappear when the
    /// final waiter leaves, while the map preserves one lock for all dispatches
    /// that overlap grant preparation or harness startup.
    dispatch_locks: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    /// A route reservation exists from the first potentially-async replacement
    /// step through harness startup. Auth changes cancel stale preparations
    /// even before a live run handle exists.
    preparations: Mutex<HashMap<String, DispatchPreparation>>,
    /// Last successfully prepared route per chat. Deliberately retained after
    /// run removal so a changed no-live dispatch still clears the authenticated
    /// upstream binding before asking for another grant.
    last_routes: Mutex<HashMap<String, RunRoute>>,
    /// chat_id → broadcast hub (retained across runs so subscribers survive turns).
    hubs: Mutex<HashMap<String, broadcast::Sender<JournaledEvent>>>,
    statuses: Mutex<HashMap<String, Session>>,
    sessions_tx: watch::Sender<Vec<Session>>,
    /// Last dispatched request per chat — the steer→new-turn fallback re-derives its
    /// run config from this (chat config rows land with the workspace doc in M4).
    last_requests: Mutex<HashMap<String, RunRequest>>,
    /// Harness-native session ids per chat (resume continuity across turns) —
    /// the live-process cache over the durable copy on the workspace chat row
    /// (comet kept the same pair on `chats.harness_session_id`). An empty
    /// session id is the "do not resume" tombstone after a rejected resume.
    harness_sessions: Mutex<HashMap<String, HarnessSessionRef>>,
    /// One local live waiter per `(source chat, thread)`. Intentionally process-local:
    /// the command ledger remains the only durable outbox.
    peer_waiters: Mutex<HashMap<(String, String), LivePeerWaiter>>,
    /// Deterministic local auto-titler, wired at engine assembly; absent in bare tests.
    titles: OnceLock<crate::titles::TitleGenerator>,
    /// Per-run loopback inference routes backed by centrally held Agent Auth credentials.
    inference_relay: OnceLock<crate::inference_relay::InferenceRelay>,
    /// Synchronous authenticated identity used to bind persistent run routes.
    auth: OnceLock<crate::Auth>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct SessionsEngine {
    inner: Arc<Inner>,
}

impl SessionsEngine {
    pub fn new(
        device_id: String,
        journal: Arc<RunJournal>,
        registry: Arc<HarnessRegistry>,
        ipc_port: u16,
    ) -> Self {
        let (sessions_tx, _) = watch::channel(Vec::new());
        Self {
            inner: Arc::new(Inner {
                device_id,
                journal,
                registry,
                ipc_port,
                doc_host: OnceLock::new(),
                runs: Mutex::new(HashMap::new()),
                route_restarts: Mutex::new(HashMap::new()),
                dispatch_locks: Mutex::new(HashMap::new()),
                preparations: Mutex::new(HashMap::new()),
                last_routes: Mutex::new(HashMap::new()),
                hubs: Mutex::new(HashMap::new()),
                statuses: Mutex::new(HashMap::new()),
                sessions_tx,
                last_requests: Mutex::new(HashMap::new()),
                harness_sessions: Mutex::new(HashMap::new()),
                peer_waiters: Mutex::new(HashMap::new()),
                titles: OnceLock::new(),
                inference_relay: OnceLock::new(),
                auth: OnceLock::new(),
            }),
        }
    }

    /// Wire the doc host (called once at engine assembly; the two services are mutually
    /// referential by design — sessions stream into docs, docs execute commands here).
    pub fn set_doc_host(&self, host: DocHost) {
        let _ = self.inner.doc_host.set(host);
    }

    /// Wire the local chat auto-titler (called once at engine assembly).
    pub fn set_titles(&self, titles: crate::titles::TitleGenerator) {
        let _ = self.inner.titles.set(titles);
    }

    pub(crate) fn set_inference_relay(&self, relay: crate::inference_relay::InferenceRelay) {
        let mut expired_routes = relay.subscribe_expired_routes();
        if self.inner.inference_relay.set(relay).is_err() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                match expired_routes.recv().await {
                    Ok(expired_route) => {
                        let Some(inner) = weak.upgrade() else {
                            break;
                        };
                        tokio::spawn(async move {
                            SessionsEngine { inner }
                                .restart_expired_route(
                                    &expired_route.logical_session_id,
                                    expired_route.lifecycle_epoch,
                                )
                                .await;
                        });
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "inference route restart signals lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    pub(crate) fn set_auth(&self, auth: crate::Auth) {
        let mut state_rx = auth.watch_state();
        if self.inner.auth.set(auth).is_err() {
            return;
        }
        let mut previous = RunAuthIdentity::from(&*state_rx.borrow_and_update());
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            while state_rx.changed().await.is_ok() {
                let current = RunAuthIdentity::from(&*state_rx.borrow_and_update());
                if current == previous {
                    continue;
                }
                previous = current.clone();
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let sessions = SessionsEngine { inner };
                sessions
                    .interrupt_runs_with_stale_auth_identity(&current)
                    .await;
            }
        });
    }

    fn auth_identity(&self) -> RunAuthIdentity {
        self.inner
            .auth
            .get()
            .map(|auth| RunAuthIdentity::from(&auth.state()))
            .unwrap_or(RunAuthIdentity::Unattached)
    }

    fn dispatch_lock(&self, chat_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = lock(&self.inner.dispatch_locks);
        if let Some(dispatch_lock) = locks.get(chat_id).and_then(Weak::upgrade) {
            return dispatch_lock;
        }
        let dispatch_lock = Arc::new(AsyncMutex::new(()));
        locks.insert(chat_id.to_string(), Arc::downgrade(&dispatch_lock));
        dispatch_lock
    }

    fn reserve_preparation(&self, chat_id: &str, route: RunRoute) -> DispatchPreparationGuard {
        let id = new_id();
        let cancel = CancellationToken::new();
        let preparation = DispatchPreparation {
            id: id.clone(),
            route,
            cancel: cancel.clone(),
        };
        if let Some(previous) =
            lock(&self.inner.preparations).insert(chat_id.to_string(), preparation)
        {
            // This is only reachable if a future caller bypasses the per-chat
            // dispatch lock. Fail closed instead of leaving both preparations live.
            previous.cancel.cancel();
        }
        DispatchPreparationGuard {
            inner: Arc::downgrade(&self.inner),
            chat_id: chat_id.to_string(),
            id,
            cancel,
        }
    }

    fn preparation_auth_is_current(
        &self,
        preparation: &DispatchPreparationGuard,
        route: &RunRoute,
    ) -> bool {
        !preparation.cancel.is_cancelled() && self.auth_identity() == route.auth_identity
    }

    fn no_live_route_requires_rebind(&self, chat_id: &str, requested: &RunRoute) -> bool {
        // With no in-memory identity the engine may have restarted after the
        // remote route became sticky but before any local run survived. Treat
        // that state as unknown and rebind conservatively before first prepare.
        lock(&self.inner.last_routes).get(chat_id) != Some(requested)
    }

    fn doc_handle(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        let host =
            self.inner.doc_host.get().ok_or_else(|| {
                EngineError::Other("doc host not wired into sessions engine".into())
            })?;
        host.open_existing_or_local(chat_id)
    }

    /// Register before appending the outbound command, closing the immediate
    /// reply race. Only one live consumer may own a source/thread pair.
    pub fn register_peer_waiter(
        &self,
        source_chat_id: &str,
        thread_id: &str,
    ) -> Result<PeerWaitRegistration, EngineError> {
        let key = (source_chat_id.to_string(), thread_id.to_string());
        let registration_id = new_id();
        let (sender, receiver) = oneshot::channel();
        let mut waiters = lock(&self.inner.peer_waiters);
        if waiters.contains_key(&key) {
            return Err(EngineError::Other("waiter_already_registered".into()));
        }
        waiters.insert(
            key.clone(),
            LivePeerWaiter {
                registration_id: registration_id.clone(),
                sender,
            },
        );
        Ok(PeerWaitRegistration {
            inner: Arc::downgrade(&self.inner),
            key,
            registration_id,
            receiver: Some(receiver),
        })
    }

    /// Await one reply. Timeout/drop removes the local waiter but never mutates
    /// or cancels the durable peer command.
    pub async fn wait_peer_reply(
        &self,
        mut registration: PeerWaitRegistration,
        timeout: std::time::Duration,
    ) -> Option<PeerReply> {
        let receiver = registration.receiver.take()?;
        tokio::time::timeout(timeout, receiver)
            .await
            .ok()
            .and_then(Result::ok)
    }

    /// Atomically reserve the active waiter for executor completion. Once
    /// claimed, no second reply can resolve the same source/thread pair.
    pub fn claim_peer_waiter(
        &self,
        source_chat_id: &str,
        thread_id: &str,
    ) -> Option<PeerWaitClaim> {
        let waiter = lock(&self.inner.peer_waiters)
            .remove(&(source_chat_id.to_string(), thread_id.to_string()))?;
        Some(PeerWaitClaim {
            sender: waiter.sender,
        })
    }

    /// Status watch: the full session list, re-sent on every transition.
    pub fn watch_sessions(&self) -> watch::Receiver<Vec<Session>> {
        self.inner.sessions_tx.subscribe()
    }

    pub fn session_status(&self, chat_id: &str) -> Option<Session> {
        lock(&self.inner.statuses).get(chat_id).cloned()
    }

    /// Any run currently working or blocked on input — the auto-updater's
    /// "don't restart from under a session" gate.
    pub fn any_active(&self) -> bool {
        lock(&self.inner.statuses).values().any(|s| {
            matches!(
                s.status,
                comet_proto::SessionStatus::Working | comet_proto::SessionStatus::AwaitingInput
            )
        })
    }

    /// The last request dispatched for a chat (steer→new-turn fallback).
    pub fn last_request(&self, chat_id: &str) -> Option<RunRequest> {
        lock(&self.inner.last_requests).get(chat_id).cloned()
    }

    /// Subscribe to a chat's live event stream: returns the journal replay after
    /// `after_seq` plus a live receiver. Subscribe-then-replay ordering means overlap
    /// (dedupe by seq) rather than gaps.
    pub fn subscribe(
        &self,
        chat_id: &str,
        after_seq: u64,
    ) -> Result<(Vec<JournaledEvent>, broadcast::Receiver<JournaledEvent>), EngineError> {
        let rx = {
            let mut hubs = lock(&self.inner.hubs);
            hubs.entry(chat_id.to_string())
                .or_insert_with(|| broadcast::channel(1024).0)
                .subscribe()
        };
        let replay = self
            .inner
            .journal
            .replay(chat_id, after_seq)?
            .into_iter()
            .map(|(seq, event)| JournaledEvent { seq, event })
            .collect();
        Ok((replay, rx))
    }

    /// One tool invocation's full input/output, reduced from the chat's run
    /// journal (the doc only carries the sanitized chip line — see
    /// [`render_parts`]). Last-write-wins on repeated ids, mirroring the
    /// fold's refresh-in-place rule. `None` when the journal never saw the
    /// call (imported/foreign sessions).
    pub fn tool_call_detail(
        &self,
        chat_id: &str,
        tool_id: &str,
    ) -> Result<Option<ToolCallDetail>, EngineError> {
        let mut detail: Option<ToolCallDetail> = None;
        for (_, event) in self.inner.journal.replay(chat_id, 0)? {
            match event {
                AgentEvent::ToolCall { ref id, ref call } if id == tool_id => {
                    let slot = detail.get_or_insert_with(ToolCallDetail::default);
                    slot.input = comet_proto::view::tool_call_input_text(call);
                }
                AgentEvent::ToolResult {
                    ref id,
                    is_error,
                    ref output,
                } if id == tool_id => {
                    let slot = detail.get_or_insert_with(ToolCallDetail::default);
                    slot.resolved = true;
                    slot.is_error = is_error;
                    if output.is_some() {
                        slot.output = output.clone();
                    }
                }
                _ => {}
            }
        }
        Ok(detail)
    }

    /// Start (or route) a run for `chat_id`.
    ///
    /// - The user message entry is written to the doc immediately (id = `message_id`).
    /// - A live steerable run receives the prompt as its next turn via the mailbox
    ///   (comet's persistent-session routing); otherwise any live run is interrupted
    ///   first — never two runtimes driving one chat.
    pub async fn dispatch(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
    ) -> Result<String, EngineError> {
        self.dispatch_with(chat_id, harness_id, request, message_id, true)
            .await
    }

    /// [`Self::dispatch`] with resume injection controllable: the failed-resume
    /// retry re-dispatches with `inject_resume = false` so a session id the
    /// harness just rejected can never be re-injected from the journal.
    /// Boxed future: `drive_run` re-enters this for that retry, and the
    /// erasure breaks the opaque-type cycle the recursion would otherwise form.
    fn dispatch_with<'a>(
        &'a self,
        chat_id: &'a str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
        inject_resume: bool,
    ) -> futures::future::BoxFuture<'a, Result<String, EngineError>> {
        Box::pin(self.dispatch_inner(chat_id, harness_id, request, message_id, inject_resume))
    }

    async fn dispatch_inner(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
        inject_resume: bool,
    ) -> Result<String, EngineError> {
        let dispatch_lock = self.dispatch_lock(chat_id);
        let _dispatch_guard = dispatch_lock.lock().await;
        self.dispatch_inner_locked(chat_id, harness_id, request, message_id, inject_resume)
            .await
    }

    async fn dispatch_inner_locked(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        mut request: RunRequest,
        message_id: Option<String>,
        inject_resume: bool,
    ) -> Result<String, EngineError> {
        enum ExistingRunDecision {
            None,
            Routed {
                run_id: String,
                status: MessageStatus,
            },
            Replace {
                run_id: String,
                route_changed: bool,
            },
        }

        let user_id = message_id.unwrap_or_else(new_id);
        let requested_route = RunRoute::new(harness_id, &request, self.auth_identity());
        let existing = {
            let mut runs = lock(&self.inner.runs);
            match runs.get_mut(chat_id) {
                None => ExistingRunDecision::None,
                Some(handle) if handle.route != requested_route => ExistingRunDecision::Replace {
                    run_id: handle.run_id.clone(),
                    route_changed: true,
                },
                Some(handle) => {
                    let was_turn_active = handle.turn_active;
                    handle.turn_active = true;
                    if !handle.steerable {
                        ExistingRunDecision::Replace {
                            run_id: handle.run_id.clone(),
                            route_changed: false,
                        }
                    } else {
                        let message = SteerMessage {
                            prompt: request.prompt.clone(),
                            message_id: Some(user_id.clone()),
                        };
                        if handle.steer_tx.try_send(message.clone()).is_err() {
                            ExistingRunDecision::Replace {
                                run_id: handle.run_id.clone(),
                                route_changed: false,
                            }
                        } else {
                            let status = handle.mailbox_message_status(was_turn_active, message);
                            handle.user_message_id = user_id.clone();
                            ExistingRunDecision::Routed {
                                run_id: handle.run_id.clone(),
                                status,
                            }
                        }
                    }
                }
            }
        };
        if let ExistingRunDecision::Routed { run_id, status } = &existing {
            lock(&self.inner.last_requests).insert(chat_id.to_string(), request.clone());
            let handle = self.doc_handle(chat_id)?;
            handle.write_user_message_with_status(&user_id, &request.prompt, now_ms(), *status)?;
            // The optimistic echo may already exist with a composer-guessed
            // steer status. Stamp the engine-authoritative state over it.
            handle.doc_arc().set_message_status(&user_id, *status)?;
            // Working BEFORE the lastMessageAt bump: both ride the
            // workspace doc from this one peer, so causal order makes it
            // impossible for an observer to hold [new message, old status]
            // — that gap read as unseen-with-no-live-run = a phantom
            // "completed" flash on every remote send (2026-07-31).
            self.set_status(chat_id, SessionStatus::Working, false);
            self.inner.note_message(chat_id, &request.prompt);
            return Ok(run_id.clone());
        }

        // Reserve before the first async teardown/rebind/prepare step. A second
        // dispatch for this chat cannot pass the lock above, and an auth change
        // can cancel this route even though no replacement RunHandle exists yet.
        let preparation = self.reserve_preparation(chat_id, requested_route.clone());
        let mut rebound = false;
        match existing {
            ExistingRunDecision::Replace {
                run_id,
                route_changed,
            } => {
                // Mailbox closed, non-steering harness, or route mismatch:
                // settle the old run before issuing any replacement.
                self.interrupt(chat_id).await?;
                // `interrupt` is intentionally bounded for ordinary callers;
                // route replacement must also await relay removal and upstream
                // grant revocation, which happen before the handle disappears.
                while self.is_live(chat_id, &run_id) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                if route_changed && let Some(relay) = self.inner.inference_relay.get() {
                    if !self.preparation_auth_is_current(&preparation, &requested_route) {
                        return Err(EngineError::Other(
                            "authenticated route changed during dispatch preparation".into(),
                        ));
                    }
                    relay.rebind(chat_id).await?;
                    rebound = true;
                }
            }
            ExistingRunDecision::None => {}
            ExistingRunDecision::Routed { .. } => unreachable!("routed dispatch returned above"),
        }

        if !self.preparation_auth_is_current(&preparation, &requested_route) {
            return Err(EngineError::Other(
                "authenticated route changed during dispatch preparation".into(),
            ));
        }
        if !rebound
            && self.no_live_route_requires_rebind(chat_id, &requested_route)
            && let Some(relay) = self.inner.inference_relay.get()
        {
            // This includes restart state where the exact prior identity is
            // unavailable. Rebind is deliberately before prepare/grant issuance.
            relay.rebind(chat_id).await?;
        }
        if !self.preparation_auth_is_current(&preparation, &requested_route) {
            return Err(EngineError::Other(
                "authenticated route changed during dispatch preparation".into(),
            ));
        }

        let harness = self.inner.registry.resolve(harness_id)?;
        let inference = if let Some(relay) = self.inner.inference_relay.get() {
            relay
                .prepare(
                    chat_id,
                    harness_id,
                    request.model.as_deref(),
                    request.agent_account_id.as_deref(),
                )
                .await?
        } else {
            None
        };
        if !self.preparation_auth_is_current(&preparation, &requested_route) {
            if let (Some(relay), Some(route)) =
                (self.inner.inference_relay.get(), inference.as_ref())
            {
                relay.remove(&route.token).await;
                relay.rebind(chat_id).await?;
            }
            return Err(EngineError::Other(
                "authenticated route changed during dispatch preparation".into(),
            ));
        }
        lock(&self.inner.last_routes).insert(chat_id.to_string(), requested_route.clone());
        let inference_token = inference.as_ref().map(|route| route.token.clone());
        let handle = self.doc_handle(chat_id)?;
        handle.write_user_message(&user_id, &request.prompt, now_ms())?;

        // Engine-owned resume (comet sessions.ts:736 — every dispatch read the
        // chat's stored harness session): callers always send `resume: None`;
        // the engine threads the chat's prior harness session back in so a new
        // process (app restart) continues the same harness conversation.
        let mut resume_injected = false;
        if request.resume.is_none() && inject_resume {
            request.resume = self.inner.resume_for(chat_id, &request.cwd);
            resume_injected = request.resume.is_some();
        }
        lock(&self.inner.last_requests).insert(chat_id.to_string(), request.clone());

        let run_id = new_id();
        let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(32);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (engine_tx, engine_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let pending_inputs: PendingInputs = Arc::new(Mutex::new(HashMap::new()));

        // Input bridge: the harness asks questions; we mint the request id, park the
        // resolver for `respond_input`, and surface the event through the run pipeline.
        let request_input = {
            let pending = pending_inputs.clone();
            let engine_tx = engine_tx.clone();
            Box::new(move |questions: Vec<UserInputQuestion>| {
                let (tx, rx) = oneshot::channel();
                let request_id = new_id();
                lock(&pending).insert(request_id.clone(), tx);
                let _ = engine_tx.send(AgentEvent::InputRequested {
                    request_id,
                    questions,
                });
                rx
            })
        };
        let interrupt_token = CancellationToken::new();
        let controls = RunControls {
            request_input,
            steering: steer_rx,
            interrupt: interrupt_token.clone(),
            context: Some(RunContext {
                session_id: chat_id.to_string(),
                ipc_port: self.inner.ipc_port,
                inference,
            }),
        };

        if !self.preparation_auth_is_current(&preparation, &requested_route) {
            if let (Some(relay), Some(token)) =
                (self.inner.inference_relay.get(), inference_token.as_deref())
            {
                relay.remove(token).await;
                relay.rebind(chat_id).await?;
            }
            return Err(EngineError::Other(
                "authenticated route changed during dispatch preparation".into(),
            ));
        }

        lock(&self.inner.runs).insert(
            chat_id.to_string(),
            RunHandle {
                user_message_id: user_id.clone(),
                run_id: run_id.clone(),
                route: requested_route,
                steerable: harness.supports_steering(),
                steering_mode: harness.steering_mode(),
                steer_tx,
                turn_active: true,
                queued_followups: VecDeque::new(),
                pending_turn_boundary_steers: VecDeque::new(),
                interrupt_token,
                cancel: cancel_tx,
                engine_tx,
                pending_inputs,
            },
        );
        self.set_status(chat_id, SessionStatus::Working, true);
        // AFTER Working (same causal-order guarantee as the steer path): the
        // lastMessageAt bump must never be observable ahead of the live run.
        self.inner.note_message(chat_id, &request.prompt);

        // Name the chat immediately from its first prompt. This is entirely
        // local and never starts an auxiliary harness/model session.
        if let Some(titles) = self.inner.titles.get() {
            titles.maybe_generate(chat_id, &request.prompt);
        }
        // Starting the harness is part of dispatch, not background run
        // consumption. A spawn/transport error must return to the durable
        // command executor so it can write Rejected + an audit reason instead
        // of first reporting Applied and failing moments later.
        let stream = match harness.run(request.clone(), controls).await {
            Ok(stream) => stream,
            Err(err) => {
                self.inner.mark_run_tearing_down(chat_id, &run_id);
                if let (Some(relay), Some(token)) =
                    (self.inner.inference_relay.get(), inference_token.as_deref())
                {
                    relay.remove(token).await;
                }
                let message = err.to_string();
                let error_event = AgentEvent::Error {
                    message: message.clone(),
                };
                let done_event = AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message),
                    session_id: None,
                };
                self.inner.publish(chat_id, &error_event);
                self.inner.publish(chat_id, &done_event);

                // A harness can reject a run before returning its event stream
                // (for example, when another process still owns a native OMP
                // session). `drive_run` cannot fold that error, so persist the
                // same visible error part here instead of leaving the composer
                // with an unexplained "Run failed" status.
                let mut folded = Vec::new();
                fold_event_into_parts(&mut folded, &error_event);
                if let Err(write_err) = finish_segment(
                    &handle.doc_arc(),
                    None,
                    &new_id(),
                    &self.inner.device_id,
                    now_ms(),
                    &folded,
                    MessageStatus::Complete,
                ) {
                    tracing::warn!(
                        chat = %chat_id,
                        err = %write_err,
                        "dispatch failure transcript write failed"
                    );
                }

                self.inner.remove_run(chat_id, &run_id);
                self.inner
                    .set_status(chat_id, SessionStatus::Errored, false);
                return Err(err.into());
            }
        };

        tokio::spawn(drive_run(
            self.inner.clone(),
            chat_id.to_string(),
            run_id.clone(),
            harness,
            request,
            handle.doc_arc(),
            stream,
            engine_rx,
            cancel_rx,
            RunResumeState {
                user_message_id: user_id,
                resume_injected,
            },
            inference_token,
        ));
        Ok(run_id)
    }

    /// Push a steer prompt into the live run's mailbox. `NotSteerable` when no live
    /// steerable run exists — the caller (command executor) dispatches a new turn.
    /// A parked persistent run accepts the prompt as its next turn immediately;
    /// that is a plain delivery, never a steer of an active turn.
    pub async fn steer(
        &self,
        chat_id: &str,
        prompt: &str,
        message_id: Option<String>,
    ) -> Result<SteerOutcome, EngineError> {
        let user_id = message_id.unwrap_or_else(new_id);
        let message = SteerMessage {
            prompt: prompt.to_string(),
            message_id: Some(user_id.clone()),
        };
        let status = {
            let mut runs = lock(&self.inner.runs);
            let Some(handle) = runs.get_mut(chat_id).filter(|handle| handle.steerable) else {
                return Ok(SteerOutcome::NotSteerable);
            };
            let was_turn_active = handle.turn_active;
            handle.turn_active = true;
            if handle.steer_tx.try_send(message.clone()).is_err() {
                return Ok(SteerOutcome::NotSteerable);
            }
            handle.mailbox_message_status(was_turn_active, message)
        };
        let handle = self.doc_handle(chat_id)?;
        handle.write_user_message_with_status(&user_id, prompt, now_ms(), status)?;
        // The optimistic echo may already exist with a generic steer status.
        // Stamp the harness-authoritative state instead of preserving that guess.
        handle.doc_arc().set_message_status(&user_id, status)?;
        if status == MessageStatus::Complete {
            // Parked delivery starts the next turn now. Working must land
            // before the lastMessageAt bump (same causal-order rule as
            // dispatch — no phantom "completed" flash for remote observers).
            self.set_status(chat_id, SessionStatus::Working, false);
        }
        self.inner.note_message(chat_id, prompt);
        Ok(SteerOutcome::Accepted(status))
    }

    /// Hold a followup outside the harness mailbox until the active turn is
    /// complete. A parked persistent harness receives it immediately because
    /// it is already at the requested next-turn boundary.
    pub async fn queue(
        &self,
        chat_id: &str,
        prompt: &str,
        message_id: Option<String>,
    ) -> Result<QueueOutcome, EngineError> {
        let user_id = message_id.unwrap_or_else(new_id);
        let message = SteerMessage {
            prompt: prompt.to_string(),
            message_id: Some(user_id.clone()),
        };
        let outcome = {
            let mut runs = lock(&self.inner.runs);
            let Some(handle) = runs.get_mut(chat_id) else {
                return Ok(QueueOutcome::NotRunning);
            };
            if !handle.turn_active && handle.steerable {
                if handle.steer_tx.try_send(message.clone()).is_err() {
                    return Ok(QueueOutcome::NotRunning);
                }
                handle.turn_active = true;
                QueueOutcome::Delivered
            } else {
                handle.queued_followups.push_back(message);
                QueueOutcome::Queued
            }
        };
        let handle = self.doc_handle(chat_id)?;
        handle.write_user_message_with_status(
            &user_id,
            prompt,
            now_ms(),
            match outcome {
                QueueOutcome::Queued => MessageStatus::Queued,
                QueueOutcome::Delivered => MessageStatus::Complete,
                QueueOutcome::NotRunning => unreachable!("returned above"),
            },
        )?;
        if outcome == QueueOutcome::Delivered {
            handle
                .doc_arc()
                .set_message_status(&user_id, MessageStatus::Complete)?;
            self.set_status(chat_id, SessionStatus::Working, false);
        }
        self.inner.note_message(chat_id, prompt);
        Ok(outcome)
    }

    /// Interrupt the live run, if any. The run settles with a synthetic
    /// `Done{interrupted}` and its streaming entry stamped `aborted`; this waits
    /// (bounded) for that settlement so callers observe a consistent doc.
    pub async fn interrupt(&self, chat_id: &str) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs).get(chat_id).map(|h| {
            (
                h.run_id.clone(),
                h.interrupt_token.clone(),
                h.cancel.clone(),
                h.pending_inputs.clone(),
            )
        });
        let Some((run_id, token, cancel, pending)) = target else {
            return Ok(false);
        };
        // Unpark any blocked question FIRST (mirrors comet: harness teardown can await a
        // parked question callback — a run stuck on a question would deadlock the stop).
        let parked: Vec<_> = lock(&pending).drain().map(|(_, tx)| tx).collect();
        for tx in parked {
            let _ = tx.send(Vec::new());
        }
        // Harness-level interrupt (protocol + child teardown) …
        token.cancel();
        // … plus the engine-side grace deadline in the run task, so a harness that
        // ignores its token still settles with a synthesized Done{interrupted}.
        let _ = cancel.send(true);
        // Bounded settle wait (the run task appends Done + stamps `aborted`).
        for _ in 0..500 {
            if !self.is_live(chat_id, &run_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(true)
    }

    async fn interrupt_runs_with_stale_auth_identity(&self, current: &RunAuthIdentity) {
        // Preparations precede live handles. Cancel them first so a grant that
        // returns under a different owner/project is revoked instead of launched.
        for preparation in lock(&self.inner.preparations).values() {
            if &preparation.route.auth_identity != current {
                preparation.cancel.cancel();
            }
        }
        let runs = lock(&self.inner.runs)
            .iter()
            .filter(|(_, handle)| &handle.route.auth_identity != current)
            .map(|(chat_id, handle)| (chat_id.clone(), handle.run_id.clone()))
            .collect::<Vec<_>>();
        let interrupted =
            futures::future::join_all(runs.iter().map(|(chat_id, _)| self.interrupt(chat_id)))
                .await;
        for ((chat_id, _), result) in runs.iter().zip(interrupted) {
            if let Err(error) = result {
                tracing::warn!(
                    chat = %chat_id,
                    error = %error,
                    "auth change run interrupt failed"
                );
            }
        }
        for (chat_id, run_id) in runs {
            while self.is_live(&chat_id, &run_id) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }

    /// Resolve a pending `request_input` question set. Returns `false` when no such
    /// request is pending (unknown id, or the run already settled).
    pub fn respond_input(
        &self,
        chat_id: &str,
        request_id: &str,
        answers: Vec<UserInputAnswer>,
    ) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .map(|h| (h.pending_inputs.clone(), h.engine_tx.clone()));
        let Some((pending, engine_tx)) = target else {
            return Ok(false);
        };
        let Some(resolver) = lock(&pending).remove(request_id) else {
            return Ok(false);
        };
        let _ = resolver.send(answers);
        let _ = engine_tx.send(AgentEvent::InputResolved {
            request_id: request_id.to_string(),
        });
        Ok(true)
    }

    /// Boot recovery: for every journal whose last event is not `Done` (a run died
    /// mid-stream), stamp this device's abandoned `streaming` doc entries `aborted`
    /// with a VISIBLE "Run interrupted by engine restart" error part, close the
    /// journal with a synthetic `Done{interrupted}` — and then PICK THE RUN BACK
    /// UP: a fresh crashed turn with revival budget left is re-dispatched against
    /// the remembered harness session (comet: "not just eulogized";
    /// `MAX_AUTO_RESUME` = 3 consecutive revivals, fresh = crashed < 12h ago).
    pub fn recover_stale(&self) -> Result<usize, EngineError> {
        const MAX_AUTO_RESUME: u32 = 3;
        const RESUME_FRESH_MS: i64 = 12 * 60 * 60 * 1000;

        let stale = self.inner.journal.stale_sessions()?;
        let mut recovered = 0usize;
        for chat_id in stale {
            if lock(&self.inner.runs).contains_key(&chat_id) {
                continue; // a live run owns this journal
            }
            let handle = self.doc_handle(&chat_id)?;
            // Harness continuity first: the crashed run's session id may only
            // exist in the journal (the debounced workspace-row write may
            // never have landed) — remember it so the revived run resumes the
            // same harness conversation (comet recoverDraft, sessions.ts:538).
            if let Some((session_id, cwd)) = self.inner.journal_harness_session(&chat_id) {
                self.inner
                    .remember_harness_session(&chat_id, &session_id, &cwd);
            }
            // The revival prompt: the last user message (idempotent re-dispatch
            // under the SAME id — `write_user_message` dedupes by id, so the
            // transcript never shows a duplicate).
            let prompt = handle.doc().read_entries().ok().and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|e| e.role == MessageRole::User)
                    .and_then(|e| {
                        e.parts.iter().find_map(|p| match p {
                            MessagePart::Text { text, .. }
                            | MessagePart::TextWindow { text, .. } => {
                                Some((e.id.clone(), text.clone()))
                            }
                            _ => None,
                        })
                    })
            });
            let attempts = self.inner.journal.resume_attempts(&chat_id);
            let fresh = handle
                .doc()
                .read_entries()
                .ok()
                .and_then(|entries| {
                    entries
                        .iter()
                        .rev()
                        .find(|e| e.status == Some(MessageStatus::Streaming))
                        .map(|e| now_ms() - e.created_at < RESUME_FRESH_MS)
                })
                .unwrap_or(false);
            let will_resume = fresh && prompt.is_some() && attempts < MAX_AUTO_RESUME;

            let note = if will_resume {
                "Run interrupted by engine restart — resuming"
            } else {
                "Run interrupted by engine restart"
            };
            let done = AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: Some(note.into()),
                session_id: None,
            };
            self.inner.publish(&chat_id, &done);
            let stamped = handle.mark_abandoned_streams(note)?.len();
            self.set_status(&chat_id, SessionStatus::Idle, false);
            tracing::info!(chat = %chat_id, stamped, will_resume, attempts, "recovered stale session journal");
            recovered += 1;

            if !will_resume {
                continue;
            }
            let attempt = self.inner.journal.note_resume_attempt(&chat_id);
            let (user_id, prompt_text) = prompt.expect("gated by will_resume");
            let sessions = self.clone();
            tokio::spawn(async move {
                let Some(host) = sessions.inner.doc_host.get().cloned() else {
                    return;
                };
                let request = sessions
                    .last_request(&chat_id)
                    .or_else(|| host.request_from_chat_row(&chat_id, &prompt_text))
                    // Last resort: the journal's own cwd (comet's draft config)
                    // — a crash can predate the debounced workspace-row write.
                    .or_else(|| {
                        let (_, cwd) = sessions.inner.journal_harness_session(&chat_id)?;
                        Some(RunRequest {
                            prompt: String::new(),
                            model: None,
                            agent_account_id: None,
                            reasoning: None,
                            model_options: Default::default(),
                            cwd,
                            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                            auto_approve: true,
                            attachments: Vec::new(),
                            resume: None,
                        })
                    });
                let Some(mut request) = request else {
                    tracing::warn!(chat = %chat_id, "auto-resume skipped: no run config");
                    return;
                };
                request.prompt = prompt_text;
                request.resume = None; // dispatch re-injects the remembered session
                request.attachments = Vec::new();
                let harness_id = host.harness_for(&chat_id);
                match sessions
                    .dispatch(&chat_id, harness_id, request, Some(user_id))
                    .await
                {
                    Ok(_) => {
                        tracing::info!(chat = %chat_id, attempt, "auto-resumed crashed run")
                    }
                    Err(err) => {
                        tracing::warn!(chat = %chat_id, error = %err, "auto-resume dispatch failed")
                    }
                }
            });
        }
        Ok(recovered)
    }

    async fn restart_expired_route(&self, chat_id: &str, lifecycle_epoch: u64) {
        enum Decision {
            Duplicate,
            Restart,
            Terminate,
        }

        let dispatch_lock = self.dispatch_lock(chat_id);
        let _dispatch_guard = dispatch_lock.lock().await;
        let target = lock(&self.inner.runs).get(chat_id).map(|handle| {
            (
                handle.run_id.clone(),
                handle.route.harness,
                handle.user_message_id.clone(),
            )
        });
        let Some((run_id, harness_id, user_message_id)) = target else {
            return;
        };
        let decision = {
            let mut restarts = lock(&self.inner.route_restarts);
            match restarts.get_mut(chat_id) {
                Some(state) if lifecycle_epoch <= state.lifecycle_epoch => Decision::Duplicate,
                Some(state) if state.restart_pending => {
                    state.lifecycle_epoch = lifecycle_epoch;
                    Decision::Terminate
                }
                Some(state) => {
                    state.lifecycle_epoch = lifecycle_epoch;
                    state.restarting_from_run_id = run_id.clone();
                    state.restart_pending = true;
                    Decision::Restart
                }
                None => {
                    restarts.insert(
                        chat_id.to_string(),
                        RouteRestartState {
                            lifecycle_epoch,
                            restarting_from_run_id: run_id.clone(),
                            restart_pending: true,
                        },
                    );
                    Decision::Restart
                }
            }
        };
        match decision {
            Decision::Duplicate => return,
            Decision::Terminate => {
                tracing::error!(
                    chat = %chat_id,
                    lifecycle_epoch,
                    "replacement inference route expired before making progress"
                );
                self.inner.mark_run_tearing_down(chat_id, &run_id);
                if let Err(error) = self.interrupt(chat_id).await {
                    tracing::error!(chat = %chat_id, %error, "failed replacement run teardown failed");
                }
                return;
            }
            Decision::Restart => {}
        }

        let Some(mut request) = self.last_request(chat_id) else {
            tracing::error!(chat = %chat_id, "expired inference route has no restart request");
            return;
        };
        tracing::warn!(
            chat = %chat_id,
            run = %run_id,
            lifecycle_epoch,
            "restarting run after local inference route expired"
        );
        self.inner.mark_run_tearing_down(chat_id, &run_id);
        if let Err(error) = self.interrupt(chat_id).await {
            tracing::error!(chat = %chat_id, %error, "expired-route run teardown failed");
            return;
        }
        request.resume = None;
        request.attachments.clear();
        if let Err(error) = self
            .dispatch_inner_locked(chat_id, harness_id, request, Some(user_message_id), true)
            .await
        {
            tracing::error!(chat = %chat_id, %error, "expired-route run restart failed");
        }
    }

    /// Graceful shutdown: interrupt every live run so streaming entries settle.
    pub async fn shutdown(&self) {
        let chats: Vec<String> = lock(&self.inner.runs).keys().cloned().collect();
        for chat_id in chats {
            if let Err(err) = self.interrupt(&chat_id).await {
                tracing::warn!(chat = %chat_id, error = %err, "shutdown interrupt failed");
            }
        }
    }

    fn is_live(&self, chat_id: &str, run_id: &str) -> bool {
        lock(&self.inner.runs)
            .get(chat_id)
            .is_some_and(|h| h.run_id == run_id)
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        self.inner.set_status(chat_id, status, fresh_start);
    }
}

impl Inner {
    /// Journal + broadcast one event (the two unconditional legs of the pipeline).
    fn publish(&self, chat_id: &str, event: &AgentEvent) -> u64 {
        let seq = match self.journal.append(chat_id, event) {
            Ok(seq) => seq,
            Err(err) => {
                tracing::error!(chat = %chat_id, error = %err, "journal append failed");
                0
            }
        };
        if let Some(hub) = lock(&self.hubs).get(chat_id) {
            let _ = hub.send(JournaledEvent {
                seq,
                event: event.clone(),
            });
        }
        seq
    }

    /// Bump the session's freshness on stream activity WITHOUT a status
    /// transition. Long silent-LOOKING stretches (thinking heartbeats, a big
    /// tool input being generated) still carry events — the UI's 45s
    /// staleness gate must not flip "Working" off mid-run. Throttled: a
    /// workspace-doc mirror per delta would be far too chatty.
    fn touch_session(&self, chat_id: &str) {
        const TOUCH_THROTTLE_MS: i64 = 10_000;
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses.get_mut(chat_id) else {
                return;
            };
            let age = now
                .signed_duration_since(entry.updated_at)
                .num_milliseconds();
            if age < TOUCH_THROTTLE_MS {
                return;
            }
            entry.updated_at = now;
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let entry = statuses
                .entry(chat_id.to_string())
                .or_insert_with(|| Session {
                    chat_id: chat_id.to_string(),
                    device_id: self.device_id.clone(),
                    status,
                    started_at: None,
                    updated_at: now,
                });
            entry.status = status;
            entry.updated_at = now;
            if fresh_start {
                entry.started_at = Some(now);
            }
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            // send_replace: keep the current value fresh even with no receivers,
            // so late WatchSessions subscribers see the last transition.
            self.sessions_tx.send_replace(list);
            session
        };
        // Mirror the transition into the workspace doc's session-status row so
        // remote devices' sidebars show this run (staleness-checked client-side).
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    fn workspace(&self) -> Option<&crate::workspace_host::WorkspaceHost> {
        self.doc_host.get().and_then(|host| host.workspace())
    }

    /// Sidebar freshness: push a message-persist preview into the chat's workspace row.
    fn note_message(&self, chat_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(ws) = self.workspace() {
            ws.note_message(chat_id, text);
        }
    }

    fn advance_omp_import_watermark(&self, chat_id: &str, cwd: &str) {
        let Some(workspace) = self.workspace() else {
            return;
        };
        let Some((session_id, session_cwd)) = workspace.chat_harness_session(chat_id) else {
            return;
        };
        if session_id.trim().is_empty()
            || session_cwd
                .as_deref()
                .is_some_and(|session_cwd| !session_cwd.is_empty() && session_cwd != cwd)
        {
            return;
        }
        let Some(updated_at) = crate::local_sessions::omp_session_updated_at(&session_id, cwd)
        else {
            return;
        };
        let current = workspace
            .doc()
            .chat(chat_id)
            .ok()
            .flatten()
            .and_then(|chat| chat.last_message_at)
            .map(|at| at.timestamp_millis());
        if current.is_some_and(|current| current >= updated_at) {
            return;
        }
        if let Err(err) = workspace.set_chat_activity(chat_id, Some(updated_at), None) {
            tracing::warn!(
                chat = %chat_id,
                session = %session_id,
                error = %err,
                "OMP native import watermark write failed"
            );
        }
    }

    /// Record the chat's harness-native session id (and its cwd): live-process
    /// cache plus the durable workspace chat row — the row is what survives an
    /// engine restart (comet sessions.ts:1039).
    fn remember_harness_session(&self, chat_id: &str, session_id: &str, cwd: &str) {
        if session_id.is_empty() {
            return;
        }
        lock(&self.harness_sessions).insert(
            chat_id.to_string(),
            HarnessSessionRef {
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
            },
        );
        if let Some(ws) = self.workspace() {
            ws.set_chat_harness_session(chat_id, session_id, cwd);
        }
        if let Some(host) = self.doc_host.get() {
            host.ensure_room_for_chat(chat_id);
        }
    }

    /// A harness rejected the stored session id: tombstone it (empty string on
    /// the row, cleared cache) so no lookup source — including the journal,
    /// which still names the dead id — can re-inject it.
    fn forget_harness_session(&self, chat_id: &str) {
        lock(&self.harness_sessions).insert(
            chat_id.to_string(),
            HarnessSessionRef {
                session_id: String::new(),
                cwd: String::new(),
            },
        );
        if let Some(ws) = self.workspace() {
            ws.set_chat_harness_session(chat_id, "", "");
        }
    }

    /// The session id to resume for a run in `chat_id` launching from `cwd`
    /// (comet sessions.ts:736, looked up on every dispatch):
    /// live-process cache → workspace chat row → journal scan (the crash path
    /// where the debounced row write never landed — SessionStarted/Done events
    /// are journaled per event, flushed immediately). Cwd-gated throughout:
    /// harness session stores are keyed by cwd, so a session created elsewhere
    /// never rides `--resume`. An empty stored id is the explicit tombstone —
    /// no resume, no falling through to staler sources.
    fn resume_for(&self, chat_id: &str, cwd: &str) -> Option<String> {
        let cwd_ok = |session_cwd: &str| session_cwd.is_empty() || session_cwd == cwd;
        if let Some(known) = lock(&self.harness_sessions).get(chat_id).cloned() {
            return (!known.session_id.is_empty() && cwd_ok(&known.cwd))
                .then_some(known.session_id);
        }
        if let Some(ws) = self.workspace()
            && let Some((session_id, session_cwd)) = ws.chat_harness_session(chat_id)
        {
            return (!session_id.is_empty() && cwd_ok(session_cwd.as_deref().unwrap_or("")))
                .then_some(session_id);
        }
        let (session_id, session_cwd) = self.journal_harness_session(chat_id)?;
        // Cache the journal hit (memory + row) so later dispatches skip the scan.
        self.remember_harness_session(chat_id, &session_id, &session_cwd);
        cwd_ok(&session_cwd).then_some(session_id)
    }

    fn note_route_restart_progress(&self, chat_id: &str, run_id: &str) {
        let mut restarts = lock(&self.route_restarts);
        if let Some(state) = restarts.get_mut(chat_id)
            && state.restart_pending
            && state.restarting_from_run_id != run_id
        {
            state.restart_pending = false;
        }
    }

    /// The last harness session id named anywhere in the chat's journal, with
    /// the cwd of the `SessionStarted` that governs it. `Done.session_id`
    /// inherits the cwd of the most recent `SessionStarted` (same run).
    fn journal_harness_session(&self, chat_id: &str) -> Option<(String, String)> {
        let events = match self.journal.replay(chat_id, 0) {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "journal scan for harness session failed");
                return None;
            }
        };
        let mut current_cwd = String::new();
        let mut found: Option<(String, String)> = None;
        for (_, event) in events {
            match event {
                AgentEvent::SessionStarted {
                    session_id, cwd, ..
                } => {
                    current_cwd = cwd;
                    if !session_id.is_empty() {
                        found = Some((session_id, current_cwd.clone()));
                    }
                }
                AgentEvent::Done {
                    session_id: Some(session_id),
                    ..
                } if !session_id.is_empty() => {
                    found = Some((session_id, current_cwd.clone()));
                }
                _ => {}
            }
        }
        found
    }

    fn mark_run_tearing_down(&self, chat_id: &str, run_id: &str) {
        let mut runs = lock(&self.runs);
        if let Some(handle) = runs
            .get_mut(chat_id)
            .filter(|handle| handle.run_id == run_id)
        {
            handle.steerable = false;
        }
    }

    fn remove_run(&self, chat_id: &str, run_id: &str) {
        let mut runs = lock(&self.runs);
        if runs.get(chat_id).is_some_and(|h| h.run_id == run_id) {
            runs.remove(chat_id);
        }
    }
}

// ── run task ────────────────────────────────────────────────────────────────

/// Apply the render-parts privacy policy: strip heavy/sensitive tool inputs before doc
/// entry. Full inputs live only in the local run journal.
fn render_parts(parts: &[MessagePart]) -> Vec<MessagePart> {
    parts
        .iter()
        .map(|part| match part {
            MessagePart::Tool {
                id,
                call,
                is_error,
                resolved,
            } => MessagePart::Tool {
                id: id.clone(),
                call: sanitize_tool_call(call),
                is_error: *is_error,
                resolved: *resolved,
            },
            other => other.clone(),
        })
        .collect()
}

/// The persisted assistant text of a folded segment (workspace preview source).
fn folded_text(parts: &[MessagePart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } | MessagePart::TextWindow { text, .. } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sync_segment<'a>(
    doc: &'a SessionDoc,
    writer: &mut Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
) -> Result<(), DocError> {
    if folded.is_empty() {
        return Ok(());
    }
    let rendered = render_parts(folded);
    if writer.is_none() {
        *writer = Some(SegmentWriter::begin(doc, entry_id, device_id, started_at)?);
    }
    if let Some(w) = writer.as_mut() {
        w.sync(&rendered)?;
    }
    Ok(())
}

fn finish_segment<'a>(
    doc: &'a SessionDoc,
    writer: Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
    status: MessageStatus,
) -> Result<(), DocError> {
    let rendered = render_parts(folded);
    match writer {
        Some(w) => w.finish(&rendered, status),
        None if !folded.is_empty() => {
            SegmentWriter::begin(doc, entry_id, device_id, started_at)?.finish(&rendered, status)
        }
        None => Ok(()),
    }
}

/// Resume bookkeeping for one run task: which user entry the run answers (so a
/// failed-resume retry re-dispatches idempotently against the same doc entry)
/// and whether `dispatch` injected the resume id itself (only engine-injected
/// resumes are retried fresh — a caller-specified resume fails loudly).
struct RunResumeState {
    user_message_id: String,
    resume_injected: bool,
}

#[allow(clippy::too_many_arguments)]
async fn drive_run(
    inner: Arc<Inner>,
    chat_id: String,
    run_id: String,
    harness: Arc<dyn Harness>,
    request: RunRequest,
    doc: Arc<SessionDoc>,
    mut stream: BoxStream<'static, Result<AgentEvent, HarnessError>>,
    mut engine_rx: mpsc::UnboundedReceiver<AgentEvent>,
    mut cancel_rx: watch::Receiver<bool>,
    resume_state: RunResumeState,
    inference_token: Option<String>,
) {
    let device_id = inner.device_id.clone();
    // Retained for resume ownership and the one-shot failed-resume retry.
    let harness_id = harness.id();
    let user_prompt = request.prompt.clone();
    let run_cwd = request.cwd.clone();
    // Kept whole for the failed-resume retry (fresh session, same user entry).
    // Option so the retry branch (inside the event loop) can take ownership.
    let mut retry_request = Some(RunRequest {
        resume: None,
        ..request.clone()
    });

    let doc_ref: &SessionDoc = &doc;
    let mut folded: Vec<MessagePart> = Vec::new();
    let mut entry_id = new_id();
    let mut segment_started = now_ms();
    let mut writer: Option<SegmentWriter<'_>> = None;
    let mut dirty = false;
    let mut flush_at = tokio::time::Instant::now();
    // Set when the engine interrupts the run: the harness gets this long to end its own
    // stream (its token was cancelled); past it, a terminal Done is synthesized.
    let mut interrupt_deadline: Option<tokio::time::Instant> = None;
    let mut interrupted = false;
    let mut saw_session_started = false;
    // Liveness heartbeat: this loop RUNNING is proof the harness stream is
    // open, so freshness must not depend on events arriving. Silent stretches
    // are normal and UNBOUNDED — a long tool call, redacted thinking, an
    // agent waiting on an external process, a question parked for an hour —
    // and each starved the UI's 45s staleness gate in turn (working strip /
    // AwaitingInput dot vanishing mid-run, both user-reported). No stall
    // timeout here by design (a first port was rejected — agents may
    // legitimately be quiet for >10min): a live child means Working, dying
    // paths each carry their own error, and engine death stops these ticks
    // so the gate still catches real crashes. touch_session throttles at 10s.
    let mut live_heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    live_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // PERSISTENT SESSION (comet runsBySession): a completed turn on a
    // steerable harness parks here instead of ending the run — the child and
    // its steering mailbox stay warm, and the next user message (dispatch
    // routes into a live run) starts the next turn with zero respawn/resume
    // latency. `Some(when)` = idle since then; the 30-min reaper below ends
    // a session nobody comes back to (comet SESSION_IDLE_MS).
    const SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
    let mut idle_since: Option<tokio::time::Instant> = None;
    let steerable = harness.supports_steering();

    let final_status = loop {
        let event: AgentEvent = tokio::select! {
            biased;
            changed = cancel_rx.changed(), if !interrupted => {
                let _ = changed;
                interrupted = true;
                interrupt_deadline = Some(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                );
                continue;
            }
            _ = tokio::time::sleep_until(
                interrupt_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if interrupt_deadline.is_some() => AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: None,
                session_id: None,
            },
            _ = live_heartbeat.tick() => {
                inner.touch_session(&chat_id);
                continue;
            }
            // Idle reaper (comet SESSION_IDLE_MS): a parked persistent session
            // nobody returned to in 30 minutes releases its child. The turn
            // was finalized at Done, so this end is clean — no aborted stamp.
            _ = tokio::time::sleep_until(
                idle_since.map(|at| at + SESSION_IDLE).unwrap_or_else(tokio::time::Instant::now)
            ), if idle_since.is_some() => {
                tracing::info!(chat = %chat_id, "reaping idle persistent session");
                if let Some(token) = lock(&inner.runs)
                    .get(&chat_id)
                    .filter(|h| h.run_id == run_id)
                    .map(|h| h.interrupt_token.clone())
                {
                    token.cancel();
                }
                break SessionStatus::Idle;
            }
            Some(event) = engine_rx.recv() => event,
            next = stream.next() => match next {
                Some(Ok(event)) => event,
                Some(Err(err)) => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(err.to_string()),
                    session_id: None,
                },
                None if interrupted => AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                },
                // Stream end while PARKED idle: a per-turn adapter closing
                // after its final Done — a clean end, not a crash (the turn
                // was already finalized). Persistent adapters keep the
                // stream open and never hit this.
                None if idle_since.is_some() => break SessionStatus::Idle,
                None => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some("harness stream ended without Done".into()),
                    session_id: None,
                },
            },
            _ = tokio::time::sleep_until(flush_at), if dirty => {
                // Coalesced STREAM_COMMIT_MS tick: one doc commit per window.
                if let Err(err) = sync_segment(
                    doc_ref, &mut writer, &entry_id, &device_id, segment_started, &folded,
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "segment sync failed");
                }
                dirty = false;
                continue;
            }
        };

        // Any stream activity proves the run is alive — keep the session's
        // freshness inside the UI's 45s staleness window (throttled).
        inner.touch_session(&chat_id);
        // First event after parking idle = the next turn beginning (a routed
        // dispatch steered in): the session is Working again.
        if idle_since.take().is_some() {
            inner.set_status(&chat_id, SessionStatus::Working, true);
        }
        // Empty reasoning deltas are PURE heartbeats: redacted thinking and
        // tool-input-generation windows stream them with no text. They fold
        // to nothing, so journaling/publishing them is only noise (hundreds
        // per long turn observed) — the touch above already did their job.
        if matches!(&event, AgentEvent::ReasoningDelta { text } if text.is_empty()) {
            continue;
        }
        if matches!(
            &event,
            AgentEvent::TextDelta { .. }
                | AgentEvent::ToolCall { .. }
                | AgentEvent::Usage { .. }
                | AgentEvent::AssistantMessageCompleted { .. }
                | AgentEvent::Done {
                    status: DoneStatus::Completed,
                    ..
                }
        ) || matches!(&event, AgentEvent::ReasoningDelta { text } if !text.is_empty())
        {
            inner.note_route_restart_progress(&chat_id, &run_id);
        }

        // ACP session names are workspace metadata, not transcript content.
        // Adopt them only while Comet's local title remains provisional; the
        // title generator preserves any later manual rename.
        if let AgentEvent::SessionTitleChanged { title } = &event {
            if let Some(titles) = inner.titles.get()
                && let Err(err) = titles.adopt_harness_title(&chat_id, title)
            {
                tracing::warn!(chat = %chat_id, error = %err, "harness session title update failed");
            }
            continue;
        }

        // Failed-resume fallback: an engine-injected `--resume` naming a session
        // the harness no longer knows dies before ever starting (claude exits
        // without an init frame; codex falls back internally via thread/start).
        // Signature: errored Done, no SessionStarted, nothing streamed. Retry
        // ONCE as a fresh session against the same user entry — tombstone the
        // dead id first so no lookup source (journal included) re-injects it.
        if resume_state.resume_injected
            && !saw_session_started
            && folded.is_empty()
            && !interrupted
            && matches!(
                &event,
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    ..
                }
            )
            && let Some(retry) = retry_request.take()
        {
            tracing::warn!(
                chat = %chat_id,
                "harness rejected injected resume id; retrying as a fresh session"
            );
            inner.mark_run_tearing_down(&chat_id, &run_id);
            inner.forget_harness_session(&chat_id);
            if let (Some(relay), Some(token)) =
                (inner.inference_relay.get(), inference_token.as_deref())
            {
                relay.remove(token).await;
            }
            inner.remove_run(&chat_id, &run_id);
            let engine = SessionsEngine {
                inner: inner.clone(),
            };
            let chat = chat_id.clone();
            let message_id = resume_state.user_message_id.clone();
            tokio::spawn(async move {
                // `inject_resume = false`: the retry must start fresh. The user
                // entry write inside dispatch is idempotent by message id.
                if let Err(err) = engine
                    .dispatch_with(&chat, harness_id, retry, Some(message_id), false)
                    .await
                {
                    tracing::error!(chat = %chat, error = %err, "fresh-session retry dispatch failed");
                }
            });
            return;
        }

        // A steer boundary splits the assistant entry exactly where the fold resets.
        if let AgentEvent::Steered {
            next_assistant_message_id,
            ..
        } = &event
        {
            let acknowledged = {
                let mut runs = lock(&inner.runs);
                runs.get_mut(&chat_id).and_then(|handle| {
                    // A Steered event means the harness is producing again —
                    // e.g. it self-continued from an internally queued prompt
                    // at a turn boundary. Keep turn_active truthful so a
                    // followup arriving now is not mistaken for parked input.
                    handle.turn_active = true;
                    (handle.steering_mode == SteeringMode::TurnBoundary)
                        .then(|| handle.pending_turn_boundary_steers.pop_front())
                        .flatten()
                })
            };
            if let Some(message) = acknowledged
                && let Some(message_id) = message.message_id.as_deref()
                && let Err(err) = doc_ref.set_message_status(message_id, MessageStatus::Steered)
            {
                tracing::warn!(
                    chat = %chat_id,
                    message = %message_id,
                    error = %err,
                    "turn-boundary steer acknowledgement stamp failed"
                );
            }
            inner.publish(&chat_id, &event);
            if let Err(err) = finish_segment(
                doc_ref,
                writer.take(),
                &entry_id,
                &device_id,
                segment_started,
                &folded,
                MessageStatus::Complete,
            ) {
                tracing::warn!(chat = %chat_id, error = %err, "segment finish failed");
            }
            inner.note_message(&chat_id, &folded_text(&folded));
            folded.clear();
            dirty = false;
            entry_id = next_assistant_message_id.clone().unwrap_or_else(new_id);
            segment_started = now_ms();
            continue;
        }

        match &event {
            AgentEvent::SessionStarted {
                session_id, cwd, ..
            } => {
                saw_session_started = true;
                // The event's own cwd (where the harness actually created the
                // session) scopes the stored id, not the request's.
                inner.remember_harness_session(&chat_id, session_id, cwd);
            }
            AgentEvent::Done {
                session_id: Some(session_id),
                ..
            } => {
                inner.remember_harness_session(&chat_id, session_id, &run_cwd);
            }
            AgentEvent::InputRequested { request_id, .. } => {
                // The engine's input bridge is the sole authority on input
                // requests: it mints the id and parks the resolver BEFORE
                // emitting the event, so a legitimate id is always pending
                // here. A harness emitting its own copy (a different id no
                // resolver knows) would fold an unanswerable twin chip into
                // the doc — and answering the twin would never resume the
                // run. Drop such events.
                let pending = lock(&inner.runs)
                    .get(&chat_id)
                    .map(|h| h.pending_inputs.clone());
                let known = pending.is_some_and(|p| lock(&p).contains_key(request_id));
                if !known {
                    tracing::warn!(
                        chat = %chat_id,
                        request = %request_id,
                        "dropping harness-emitted InputRequested (unknown id; \
                         the engine input bridge owns this lifecycle)"
                    );
                    continue;
                }
                inner.set_status(&chat_id, SessionStatus::AwaitingInput, false);
            }
            AgentEvent::InputResolved { .. } => {
                inner.set_status(&chat_id, SessionStatus::Working, false);
            }
            _ => {}
        }

        inner.publish(&chat_id, &event);

        // Defensive rule from comet: a mid-run SessionStarted re-emission (Claude SDK
        // background re-invocations) must not wipe the segment being written.
        let skip_fold = matches!(&event, AgentEvent::SessionStarted { .. }) && !folded.is_empty();
        if !skip_fold {
            fold_event_into_parts(&mut folded, &event);
        }

        if let AgentEvent::Done { status, .. } = &event {
            let message_status = match status {
                DoneStatus::Interrupted => MessageStatus::Aborted,
                DoneStatus::Completed | DoneStatus::Errored => MessageStatus::Complete,
            };
            // No dangling chips: a run that ends for ANY reason (completed,
            // errored, interrupted) terminally resolves its input parts — an
            // unresolved question must not outlive the run that asked it
            // (its resolver died with the run; an answer could never land).
            for part in folded.iter_mut() {
                if let MessagePart::Input { resolved, .. } = part {
                    *resolved = true;
                }
            }
            // A Done landing on a PARKED session with nothing streamed (the
            // idle reaper's or an interrupt's own teardown) has no entry to
            // finalize — writing one would leave an empty aborted stub.
            let nothing_streamed = writer.is_none() && folded.is_empty();
            if !nothing_streamed {
                if let Err(err) = finish_segment(
                    doc_ref,
                    writer.take(),
                    &entry_id,
                    &device_id,
                    segment_started,
                    &folded,
                    message_status,
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "final segment finish failed");
                }
                inner.note_message(&chat_id, &folded_text(&folded));
            }
            if harness_id == HarnessId::Omp {
                inner.advance_omp_import_watermark(&chat_id, &run_cwd);
            }
            if *status == DoneStatus::Completed {
                // A cleanly completed turn resets the auto-resume revival
                // budget: only consecutive crash-revive-crash cycles spend it.
                inner.journal.clear_resume_attempts(&chat_id);
            }
            // Retry local titling after a completed exchange in case the
            // dispatch-time task could not observe the chat row yet.
            if *status == DoneStatus::Completed
                && let Some(titles) = inner.titles.get()
            {
                titles.maybe_generate(&chat_id, &user_prompt);
            }
            let persistent_boundary = *status == DoneStatus::Completed && steerable && !interrupted;
            let (queued_delivery, queue_transport_open) = {
                let mut runs = lock(&inner.runs);
                match runs.get_mut(&chat_id) {
                    Some(handle) => {
                        handle.turn_active = false;
                        if persistent_boundary {
                            if let Some(followup) = handle.queued_followups.pop_front() {
                                if handle.steer_tx.try_send(followup.clone()).is_ok() {
                                    handle.turn_active = true;
                                    (Some(followup), true)
                                } else {
                                    handle.queued_followups.push_front(followup);
                                    (None, false)
                                }
                            } else {
                                (None, true)
                            }
                        } else {
                            (None, true)
                        }
                    }
                    None => (None, false),
                }
            };

            // PERSISTENT SESSION: a cleanly completed turn on a steerable
            // harness parks instead of ending. Explicit queue messages cross
            // into the harness mailbox only here, after Done.
            if persistent_boundary && queue_transport_open {
                folded.clear();
                dirty = false;
                entry_id = new_id();
                segment_started = now_ms();
                // Resume-retry is strictly a first-turn concern.
                saw_session_started = true;
                idle_since = Some(tokio::time::Instant::now());
                if let Some(followup) = queued_delivery {
                    if let Some(message_id) = followup.message_id.as_deref()
                        && let Err(err) =
                            doc_ref.set_message_status(message_id, MessageStatus::Complete)
                    {
                        tracing::warn!(
                            chat = %chat_id,
                            message = %message_id,
                            error = %err,
                            "queued user message delivery stamp failed"
                        );
                    }
                    idle_since = None;
                    inner.set_status(&chat_id, SessionStatus::Working, false);
                } else {
                    inner.set_status(&chat_id, SessionStatus::Idle, false);
                }
                continue;
            }
            break match status {
                DoneStatus::Errored => SessionStatus::Errored,
                _ => SessionStatus::Idle,
            };
        }

        if !folded.is_empty() && !dirty {
            dirty = true;
            flush_at =
                tokio::time::Instant::now() + std::time::Duration::from_millis(STREAM_COMMIT_MS);
        }
    };

    inner.mark_run_tearing_down(&chat_id, &run_id);
    let mut deferred_followups = {
        let mut runs = lock(&inner.runs);
        runs.get_mut(&chat_id)
            .map(|handle| {
                let mut deferred = handle
                    .pending_turn_boundary_steers
                    .drain(..)
                    .collect::<VecDeque<_>>();
                deferred.extend(handle.queued_followups.drain(..));
                deferred
            })
            .unwrap_or_default()
    };
    if let (Some(relay), Some(token)) = (inner.inference_relay.get(), inference_token.as_deref()) {
        relay.remove(token).await;
    }
    inner.remove_run(&chat_id, &run_id);
    inner.set_status(&chat_id, final_status, false);

    if interrupted {
        for followup in deferred_followups {
            if let Some(message_id) = followup.message_id.as_deref()
                && let Err(err) = doc_ref.set_message_status(message_id, MessageStatus::Aborted)
            {
                tracing::warn!(
                    chat = %chat_id,
                    message = %message_id,
                    error = %err,
                    "cancelled queued user message stamp failed"
                );
            }
        }
        return;
    }

    if !deferred_followups.is_empty() {
        let engine = SessionsEngine {
            inner: inner.clone(),
        };
        let chat = chat_id.clone();
        tokio::spawn(async move {
            while let Some(followup) = deferred_followups.pop_front() {
                let prompt = followup.prompt;
                let message_id = followup.message_id;
                match engine.queue(&chat, &prompt, message_id.clone()).await {
                    Ok(QueueOutcome::Queued | QueueOutcome::Delivered) => {}
                    Ok(QueueOutcome::NotRunning) => {
                        let Some(mut request) = engine.last_request(&chat) else {
                            tracing::error!(chat = %chat, "queued followup lost its run config");
                            return;
                        };
                        request.prompt = prompt;
                        request.resume = None;
                        request.attachments.clear();
                        if let Some(message_id) = message_id.as_deref()
                            && let Ok(handle) = engine.doc_handle(&chat)
                            && let Err(err) = handle
                                .doc_arc()
                                .set_message_status(message_id, MessageStatus::Complete)
                        {
                            tracing::warn!(
                                chat = %chat,
                                message = %message_id,
                                error = %err,
                                "queued user message fallback stamp failed"
                            );
                        }
                        if let Err(err) = engine
                            .dispatch(&chat, harness_id, request, message_id)
                            .await
                        {
                            tracing::error!(
                                chat = %chat,
                                error = %err,
                                "queued followup fallback dispatch failed"
                            );
                            return;
                        }
                    }
                    Err(err) => {
                        tracing::error!(
                            chat = %chat,
                            error = %err,
                            "queued followup delivery failed"
                        );
                        return;
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use comet_proto::SessionRoomProjection;
    use comet_proto::{Model, ReasoningLevel, RuntimeProfile, SandboxLevel};
    use comet_sync::DocsStore;

    use crate::doc_host::DocHostConfig;
    use crate::workspace_host::{WorkspaceHost, WorkspaceHostConfig};

    #[tokio::test]
    async fn sessions_reuse_an_existing_projected_chat_handle() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            WorkspaceHostConfig {
                device_id: "comet-scaffold-sandbox-a-e1".into(),
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
                device_id: "comet-scaffold-sandbox-a-e1".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        host.set_workspace(workspace);
        let projected = host
            .open_projection(
                "session-a",
                Some(&SessionRoomProjection {
                    project_id: "project-a".into(),
                    deployment_id: "deployment-a".into(),
                    session_id: "session-a".into(),
                }),
            )
            .unwrap();

        let sessions = SessionsEngine::new(
            "comet-scaffold-sandbox-a-e1".into(),
            Arc::new(RunJournal::open(dir.path().join("journal")).unwrap()),
            Arc::new(HarnessRegistry::new()),
            27654,
        );
        sessions.set_doc_host(host);

        let run_handle = sessions
            .doc_handle("session-a")
            .expect("session execution should preserve the projected room");
        assert!(Arc::ptr_eq(&projected, &run_handle));
    }

    fn bare_sessions(path: &std::path::Path) -> SessionsEngine {
        SessionsEngine::new(
            "test-device".into(),
            Arc::new(RunJournal::open(path).unwrap()),
            Arc::new(HarnessRegistry::new()),
            27654,
        )
    }

    fn test_route(harness: HarnessId) -> RunRoute {
        RunRoute::new(
            harness,
            &RunRequest {
                prompt: "hello".into(),
                model: Some("gpt-5.6-sol".into()),
                agent_account_id: Some("account-a".into()),
                reasoning: None,
                model_options: Default::default(),
                cwd: "/tmp".into(),
                sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                auto_approve: false,
                resume: None,
                attachments: vec![],
            },
            RunAuthIdentity::SignedIn {
                owner_subject: "owner-a".into(),
                project_scope: "project-a".into(),
            },
        )
    }

    #[test]
    fn persistent_run_route_changes_with_harness_model_account_or_auth_identity() {
        let route = test_route(HarnessId::Codex);
        assert_eq!(route, test_route(HarnessId::Codex));
        assert_ne!(route, test_route(HarnessId::ClaudeCode));

        let mut changed_account_request = RunRequest {
            prompt: "hello".into(),
            model: Some("gpt-5.6-sol".into()),
            agent_account_id: Some("account-b".into()),
            reasoning: None,
            model_options: Default::default(),
            cwd: "/tmp".into(),
            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
            auto_approve: false,
            resume: None,
            attachments: vec![],
        };
        let identity = route.auth_identity.clone();
        assert_ne!(
            route,
            RunRoute::new(HarnessId::Codex, &changed_account_request, identity.clone())
        );

        changed_account_request.agent_account_id = None;
        assert_ne!(
            route,
            RunRoute::new(HarnessId::Codex, &changed_account_request, identity.clone())
        );

        changed_account_request.agent_account_id = Some("account-a".into());
        changed_account_request.model = Some("gpt-5.6-terra".into());
        assert_ne!(
            route,
            RunRoute::new(HarnessId::Codex, &changed_account_request, identity)
        );

        let original_request = RunRequest {
            model: Some("gpt-5.6-sol".into()),
            agent_account_id: Some("account-a".into()),
            ..changed_account_request
        };
        assert_ne!(
            route,
            RunRoute::new(
                HarnessId::Codex,
                &original_request,
                RunAuthIdentity::SignedIn {
                    owner_subject: "owner-b".into(),
                    project_scope: "project-a".into(),
                }
            )
        );
        assert_ne!(
            route,
            RunRoute::new(
                HarnessId::Codex,
                &original_request,
                RunAuthIdentity::SignedIn {
                    owner_subject: "owner-a".into(),
                    project_scope: "project-b".into(),
                }
            )
        );
        assert_ne!(
            route,
            RunRoute::new(
                HarnessId::Codex,
                &original_request,
                RunAuthIdentity::SignedOut
            )
        );
    }

    #[tokio::test]
    async fn concurrent_idle_dispatches_are_serialized_before_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = bare_sessions(&dir.path().join("journal"));
        let first_lock = sessions.dispatch_lock("chat-a");
        let first = first_lock.lock().await;
        let contender = sessions.clone();
        let (entered_tx, mut entered_rx) = oneshot::channel();
        let (attempted_tx, attempted_rx) = oneshot::channel();

        let second = tokio::spawn(async move {
            let _ = attempted_tx.send(());
            let dispatch_lock = contender.dispatch_lock("chat-a");
            let _guard = dispatch_lock.lock().await;
            let _ = entered_tx.send(());
        });
        attempted_rx
            .await
            .expect("the contender should reach the per-chat lock");
        tokio::task::yield_now().await;
        assert!(matches!(
            entered_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        drop(first);
        entered_rx
            .await
            .expect("the serialized dispatch should proceed after release");
        second.await.unwrap();
    }

    #[tokio::test]
    async fn auth_change_cancels_a_route_during_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = bare_sessions(&dir.path().join("journal"));
        let route = test_route(HarnessId::Codex);
        let preparation = sessions.reserve_preparation("chat-a", route);
        assert!(!preparation.cancel.is_cancelled());

        sessions
            .interrupt_runs_with_stale_auth_identity(&RunAuthIdentity::SignedIn {
                owner_subject: "owner-b".into(),
                project_scope: "project-b".into(),
            })
            .await;

        assert!(preparation.cancel.is_cancelled());
    }

    #[test]
    fn no_live_changed_or_restart_unknown_route_requires_rebind() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = bare_sessions(&dir.path().join("journal"));
        let route = test_route(HarnessId::Codex);

        assert!(
            sessions.no_live_route_requires_rebind("chat-a", &route),
            "restart-unknown route state must fail closed"
        );
        lock(&sessions.inner.last_routes).insert("chat-a".into(), route.clone());
        assert!(!sessions.no_live_route_requires_rebind("chat-a", &route));
        assert!(
            sessions.no_live_route_requires_rebind("chat-a", &test_route(HarnessId::ClaudeCode))
        );
        let mut changed_model = route.clone();
        changed_model.model = Some("gpt-5.6-terra".into());
        assert!(sessions.no_live_route_requires_rebind("chat-a", &changed_model));

        let mut changed_account = route.clone();
        changed_account.agent_account_id = Some("account-b".into());
        assert!(sessions.no_live_route_requires_rebind("chat-a", &changed_account));
    }
    struct RouteRestartHarness {
        requests: Arc<Mutex<Vec<RunRequest>>>,
    }

    #[async_trait]
    impl Harness for RouteRestartHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }

        fn display_name(&self) -> &str {
            "Route restart"
        }

        fn supports_steering(&self) -> bool {
            false
        }

        fn steering_mode(&self) -> SteeringMode {
            SteeringMode::TurnBoundary
        }

        fn reasoning_levels(&self) -> &[ReasoningLevel] {
            &[ReasoningLevel::Medium]
        }

        async fn models(&self) -> Result<Vec<Model>, HarnessError> {
            Ok(Vec::new())
        }

        async fn run(
            &self,
            request: RunRequest,
            controls: RunControls,
        ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
            lock(&self.requests).push(request.clone());
            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(async move {
                let _ = tx
                    .send(Ok(AgentEvent::SessionStarted {
                        harness: HarnessId::Mock,
                        model: "mock-route".into(),
                        tools: Vec::new(),
                        cwd: request.cwd,
                        session_id: "route-session".into(),
                        assistant_message_id: "route-assistant".into(),
                    }))
                    .await;
                controls.interrupt.cancelled().await;
                let _ = tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: Some("route-session".into()),
                    }))
                    .await;
            });
            Ok(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })
            .boxed())
        }
    }

    struct RouteRestartToken;

    #[async_trait]
    impl comet_rpc::TokenSource for RouteRestartToken {
        async fn token(&self) -> Option<String> {
            Some("route-restart-token".into())
        }
    }

    #[tokio::test]
    async fn expired_route_restarts_once_with_the_same_session_and_user_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path().join("store")).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            WorkspaceHostConfig {
                device_id: "device-route-restart".into(),
                device_name: "test".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "owner-a".into(),
                edge: None,
            },
        )
        .unwrap();
        workspace
            .create_space("space-route", "device-route-restart", "/tmp", None, false)
            .unwrap();
        workspace
            .create_chat("chat-route", "space-route", None, None)
            .unwrap();
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "device-route-restart".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        host.set_workspace(workspace);

        let requests = Arc::new(Mutex::new(Vec::new()));
        let registry = HarnessRegistry::for_profile(RuntimeProfile::Mock);
        registry.register(Arc::new(RouteRestartHarness {
            requests: requests.clone(),
        }));
        let sessions = SessionsEngine::new(
            "device-route-restart".into(),
            Arc::new(RunJournal::open(dir.path().join("journal")).unwrap()),
            Arc::new(registry),
            27654,
        );
        sessions.set_doc_host(host);
        sessions
            .dispatch(
                "chat-route",
                HarnessId::Mock,
                RunRequest {
                    prompt: "continue safely".into(),
                    model: None,
                    agent_account_id: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: "/tmp".into(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    auto_approve: true,
                    attachments: Vec::new(),
                    resume: None,
                },
                Some("route-user-message".into()),
            )
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while lock(&sessions.inner.harness_sessions)
            .get("chat-route")
            .is_none()
        {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let relay = crate::inference_relay::InferenceRelay::start(
            crate::scaffold::ScaffoldClient::new(
                "http://127.0.0.1:1",
                "project-a",
                Arc::new(RouteRestartToken),
            )
            .unwrap(),
        )
        .unwrap();
        sessions.set_inference_relay(relay.clone());

        relay.notify_expired_route("chat-route", 1);
        let restart_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while lock(&requests).len() < 2 {
            assert!(tokio::time::Instant::now() < restart_deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let recorded = lock(&requests).clone();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[1].prompt, "continue safely");
        assert_eq!(recorded[1].resume.as_deref(), Some("route-session"));
        assert!(recorded[1].attachments.is_empty());

        relay.notify_expired_route("chat-route", 1);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(lock(&sessions.inner.runs).contains_key("chat-route"));
        assert_eq!(lock(&requests).len(), 2, "duplicate expiry must be ignored");

        relay.notify_expired_route("chat-route", 2);
        let stop_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while lock(&sessions.inner.runs).contains_key("chat-route") {
            assert!(tokio::time::Instant::now() < stop_deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            lock(&requests).len(),
            2,
            "restart must be bounded until inference makes progress"
        );
        let entries = sessions
            .doc_handle("chat-route")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.id == "route-user-message")
                .count(),
            1,
            "restart must reuse the existing user entry"
        );
        sessions.shutdown().await;
    }
}
