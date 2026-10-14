//! One engine-owned finite Boat lease; parked harnesses never keep a host awake.
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use comet_harness::CancellationToken;
use comet_proto::{AgentEvent, DeviceEnvironment};
use futures::future::BoxFuture;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, watch};
use tokio::time::Instant;

const TTL: Duration = Duration::from_secs(3600);
const REFRESH: Duration = Duration::from_secs(300);
const SAFETY: Duration = Duration::from_secs(60);
const FAILURE: &str = "Crew lost Boat TTL protection; this turn was interrupted and remains recoverable. Check the host's Boat CLI authorization before retrying.";
pub(super) type Extend =
    Arc<dyn Fn(String, CancellationToken) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

pub(super) struct BoatLease {
    shared: Arc<Shared>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct Shared {
    binding: Result<Option<String>, String>,
    extend: Extend,
    state: Mutex<State>,
    operation: AsyncMutex<()>,
    changed: Notify,
    stop: CancellationToken,
}

#[derive(Default)]
struct State {
    turns: HashMap<String, Turn>,
    deadline: Option<Instant>,
    refresh: Option<Instant>,
    idle_reset: bool,
}

struct Turn {
    interrupt: CancellationToken,
    cancel: watch::Sender<bool>,
    events: mpsc::UnboundedSender<AgentEvent>,
    failed: bool,
}

impl BoatLease {
    pub(super) fn detect() -> Self {
        let binding = if std::env::var("COMET_DEVBOX_PROVIDER").as_deref() == Ok("boat") {
            if crate::devbox_environment() == Some(DeviceEnvironment::Boat) {
                std::env::var("COMET_DEVBOX_ID")
                    .map(Some)
                    .map_err(|_| "Boat host binding is missing".into())
            } else {
                Err("Boat host binding does not match this sandbox".into())
            }
        } else {
            Ok(None)
        };
        Self::new(
            binding,
            Arc::new(|id, cancel| {
                Box::pin(async move { crate::device_wake::run_boat_extend(&id, &cancel).await })
            }),
        )
    }

    pub(super) fn new(binding: Result<Option<String>, String>, extend: Extend) -> Self {
        Self {
            shared: Arc::new(Shared {
                binding,
                extend,
                state: Mutex::new(State::default()),
                operation: AsyncMutex::new(()),
                changed: Notify::new(),
                stop: CancellationToken::new(),
            }),
            worker: Mutex::new(None),
        }
    }

    /// Confirm protection before touching a harness/mailbox. No session lock is held.
    pub(super) async fn protect(&self) -> Result<(), String> {
        if self.shared.stop.is_cancelled() {
            return Err("Crew is shutting down".into());
        }
        let Some(_) = self.shared.binding.as_ref().map_err(Clone::clone)? else {
            return Ok(());
        };
        {
            let mut worker = self.worker.lock();
            if worker.is_none() {
                *worker = Some(tokio::spawn(worker_loop(self.shared.clone())));
            }
        }
        let _operation = tokio::select! {
            guard = self.shared.operation.lock() => guard,
            _ = self.shared.stop.cancelled() => return Err("Crew is shutting down".into()),
        };
        if self.shared.stop.is_cancelled() {
            return Err("Crew is shutting down".into());
        }
        renew(&self.shared).await
    }

    pub(super) fn start_turn(
        &self,
        run: &str,
        interrupt: &CancellationToken,
        cancel: &watch::Sender<bool>,
        events: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<(), String> {
        let Some(_) = self.shared.binding.as_ref().map_err(Clone::clone)? else {
            return Ok(());
        };
        let mut state = self.shared.state.lock();
        if self.shared.stop.is_cancelled() {
            return Err("Crew is shutting down".into());
        }
        if state
            .deadline
            .is_none_or(|at| at <= Instant::now() + SAFETY)
        {
            fail_active(&mut state);
            return Err("Crew has no confirmed Boat TTL protection".into());
        }
        if let Some(turn) = state.turns.get(run) {
            return if turn.failed {
                Err(FAILURE.into())
            } else {
                Ok(())
            };
        }
        state.turns.insert(
            run.to_owned(),
            Turn {
                interrupt: interrupt.clone(),
                cancel: cancel.clone(),
                events: events.clone(),
                failed: false,
            },
        );
        state.idle_reset = false;
        if state.refresh.is_none() {
            state.refresh = Some(Instant::now() + REFRESH);
        }
        self.shared.changed.notify_one();
        Ok(())
    }

    pub(super) fn finish_turn(&self, run: &str) {
        let mut state = self.shared.state.lock();
        if state.turns.remove(run).is_some() && state.turns.is_empty() {
            state.refresh = None;
            state.idle_reset = true;
            self.shared.changed.notify_one();
        }
    }

    pub(super) fn failed(&self, run: &str) -> bool {
        self.shared
            .state
            .lock()
            .turns
            .get(run)
            .is_some_and(|turn| turn.failed)
    }

    pub(super) fn shutdown_now(&self) {
        // Drop never performs network I/O; finite provider TTL handles crashes.
        self.shared.stop.cancel();
    }

    pub(super) async fn shutdown(&self) {
        self.shutdown_now();
        let worker = self.worker.lock().take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }
        let _operation = self.shared.operation.lock().await;
        let reset = std::mem::take(&mut self.shared.state.lock().idle_reset);
        if reset && let Ok(Some(id)) = &self.shared.binding {
            if (self.shared.extend)(id.clone(), CancellationToken::new())
                .await
                .is_err()
            {
                tracing::error!("Crew could not reset Boat idle TTL during shutdown");
            }
        }
    }
}

impl Drop for BoatLease {
    fn drop(&mut self) {
        self.shutdown_now();
    }
}

fn fail_active(state: &mut State) {
    state.deadline = None;
    state.refresh = None;
    for turn in state.turns.values_mut().filter(|turn| !turn.failed) {
        turn.failed = true;
        let _ = turn.events.send(AgentEvent::Error {
            message: FAILURE.into(),
        });
        turn.interrupt.cancel();
        let _ = turn.cancel.send(true);
    }
    tracing::error!("Crew could not confirm Boat TTL protection; active turns interrupted");
}

async fn renew(shared: &Shared) -> Result<(), String> {
    let id = shared
        .binding
        .as_ref()
        .map_err(Clone::clone)?
        .as_ref()
        .expect("Boat binding");
    {
        let mut state = shared.state.lock();
        if !state.turns.is_empty()
            && state
                .deadline
                .is_some_and(|at| at <= Instant::now() + SAFETY)
        {
            fail_active(&mut state);
            return Err(FAILURE.into());
        }
    }
    // The CLI resets TTL at the server; request start is the conservative bound.
    let started = Instant::now();
    let result = tokio::select! {
        result = (shared.extend)(id.clone(), shared.stop.clone()) => result,
        _ = shared.stop.cancelled() => return Err("Crew is shutting down".into()),
    };
    let mut state = shared.state.lock();
    if result.is_ok() && !shared.stop.is_cancelled() {
        state.deadline = Some(started + TTL - SAFETY);
        if !state.turns.is_empty() {
            state.refresh = Some(Instant::now() + REFRESH);
        }
        shared.changed.notify_one();
        Ok(())
    } else {
        fail_active(&mut state);
        Err("Crew could not confirm Boat TTL protection. Check the host's Boat CLI authorization and retry.".into())
    }
}

async fn worker_loop(shared: Arc<Shared>) {
    loop {
        let (reset, refresh) = {
            let state = shared.state.lock();
            (state.idle_reset, state.refresh)
        };
        if !reset {
            tokio::select! {
                _ = shared.stop.cancelled() => break,
                _ = shared.changed.notified() => continue,
                _ = tokio::time::sleep_until(refresh.unwrap_or_else(Instant::now)), if refresh.is_some() => {},
            }
        }
        let _operation = tokio::select! {
            guard = shared.operation.lock() => guard,
            _ = shared.stop.cancelled() => break,
        };
        let (reset, due) = {
            let mut state = shared.state.lock();
            let reset = std::mem::take(&mut state.idle_reset);
            (
                reset,
                reset || state.refresh.is_some_and(|at| at <= Instant::now()),
            )
        };
        let result = if due { renew(&shared).await } else { Ok(()) };
        if shared.stop.is_cancelled() {
            if reset && result.is_err() {
                shared.state.lock().idle_reset = true;
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    fn lease() -> (
        BoatLease,
        mpsc::UnboundedReceiver<oneshot::Sender<Result<(), String>>>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let extend: Extend = Arc::new(move |_, cancel| {
            let (reply, result) = oneshot::channel();
            tx.send(reply).unwrap();
            Box::pin(async move {
                tokio::select! {
                    result = result => result.unwrap(),
                    _ = cancel.cancelled() => Err("cancelled".into()),
                }
            })
        });
        (BoatLease::new(Ok(Some("bx_23456789".into())), extend), rx)
    }

    async fn confirm(
        lease: &BoatLease,
        commands: &mut mpsc::UnboundedReceiver<oneshot::Sender<Result<(), String>>>,
    ) {
        let protection = lease.protect();
        tokio::pin!(protection);
        tokio::select! {
            reply = commands.recv() => reply.unwrap().send(Ok(())).unwrap(),
            result = &mut protection => panic!("admitted before protection: {result:?}"),
        }
        protection.await.unwrap();
    }

    fn start(
        lease: &BoatLease,
        run: &str,
    ) -> (
        CancellationToken,
        watch::Receiver<bool>,
        mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let token = CancellationToken::new();
        let (cancel, rx) = watch::channel(false);
        let (events, event_rx) = mpsc::unbounded_channel();
        lease.start_turn(run, &token, &cancel, &events).unwrap();
        (token, rx, event_rx)
    }

    #[tokio::test(start_paused = true)]
    async fn overlap_renews_and_last_finish_resets_once_without_idle_renewal() {
        let (lease, mut commands) = lease();
        confirm(&lease, &mut commands).await;
        let (a, _, _) = start(&lease, "a");
        let (b, _, _) = start(&lease, "b");
        start(&lease, "b");
        lease.finish_turn("a");
        lease.finish_turn("a");
        for _ in 0..13 {
            tokio::time::advance(REFRESH).await;
            commands.recv().await.unwrap().send(Ok(())).unwrap();
            tokio::task::yield_now().await;
        }
        assert!(!a.is_cancelled() && !b.is_cancelled());
        lease.finish_turn("b");
        commands.recv().await.unwrap().send(Ok(())).unwrap();
        tokio::task::yield_now().await;
        lease.finish_turn("b");
        tokio::time::advance(TTL * 2).await;
        assert!(commands.try_recv().is_err());
        lease.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn failed_admission_and_renewal_fail_closed_and_signal_bounded_interrupt() {
        let (lease, mut commands) = lease();
        let protection = lease.protect();
        tokio::pin!(protection);
        tokio::select! {
            reply = commands.recv() => reply.unwrap().send(Err("denied".into())).unwrap(),
            _ = &mut protection => panic!("admitted before CLI completion"),
        }
        assert!(protection.await.is_err());
        assert!(
            lease
                .start_turn(
                    "unprotected",
                    &CancellationToken::new(),
                    &watch::channel(false).0,
                    &mpsc::unbounded_channel().0
                )
                .is_err()
        );
        confirm(&lease, &mut commands).await;
        let (token, cancel, mut events) = start(&lease, "active");
        tokio::time::advance(REFRESH).await;
        commands
            .recv()
            .await
            .unwrap()
            .send(Err("denied".into()))
            .unwrap();
        tokio::task::yield_now().await;
        assert!(token.is_cancelled() && *cancel.borrow());
        assert!(lease.failed("active"));
        assert!(matches!(
            events.recv().await,
            Some(AgentEvent::Error { .. })
        ));
        tokio::time::advance(TTL * 2).await;
        assert!(
            commands.try_recv().is_err(),
            "no unbounded retry after failure"
        );
        lease.finish_turn("active");
        commands.recv().await.unwrap().send(Ok(())).unwrap();
        tokio::task::yield_now().await;
        lease.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn missed_deadline_cancels_instead_of_silently_recovering_running_work() {
        let (lease, mut commands) = lease();
        confirm(&lease, &mut commands).await;
        let (token, cancel, _) = start(&lease, "active");
        tokio::time::advance(TTL).await;
        tokio::task::yield_now().await;
        assert!(token.is_cancelled() && *cancel.borrow());
        assert!(lease.failed("active"));
        assert!(commands.try_recv().is_err());
        lease.finish_turn("active");
        commands.recv().await.unwrap().send(Ok(())).unwrap();
        tokio::task::yield_now().await;
        lease.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_pending_admission_and_rejects_restart() {
        let (lease, mut commands) = lease();
        let protection = lease.protect();
        tokio::pin!(protection);
        let reply = tokio::select! {
            reply = commands.recv() => reply.unwrap(),
            _ = &mut protection => panic!("premature admission"),
        };
        lease.shutdown_now();
        assert!(protection.await.is_err());
        assert!(reply.send(Ok(())).is_err());
        assert!(lease.protect().await.is_err());
        lease.shutdown().await;
    }
}
