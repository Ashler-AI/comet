//! Finite Boat protection belongs to active turns, never parked harnesses.
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, watch};
use tokio::time::Instant;
use zeron_harness::CancellationToken;
use zeron_proto::AgentEvent;

const TTL: Duration = Duration::from_secs(3600);
const REFRESH: Duration = Duration::from_secs(300);
const SAFETY: Duration = Duration::from_secs(60);
pub(crate) const FAILURE: &str = "Crew lost Boat TTL protection; this turn was interrupted and remains recoverable. Check the host's Boat CLI authorization before retrying.";
type Extend =
    Arc<dyn Fn(String, CancellationToken) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

pub(crate) struct BoatLease {
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

/// Also covers failed admission, task cancellation and startup-error returns.
pub(crate) struct BoatRun {
    shared: Arc<Shared>,
    run: String,
    interrupt: CancellationToken,
}

impl Drop for BoatRun {
    fn drop(&mut self) {
        self.interrupt.cancel();
        finish_turn(&self.shared, &self.run);
    }
}

impl BoatLease {
    pub(crate) fn detect() -> Self {
        Self::new(
            crate::devbox::boat_binding(),
            Arc::new(|id, cancel| {
                Box::pin(async move { crate::devbox::run_boat_extend(&id, &cancel).await })
            }),
        )
    }

    pub(crate) fn new(binding: Result<Option<String>, String>, extend: Extend) -> Self {
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

    pub(crate) fn run_guard(&self, run: &str, interrupt: &CancellationToken) -> Option<BoatRun> {
        self.shared.binding.as_ref().ok()?.as_ref()?;
        Some(BoatRun {
            shared: self.shared.clone(),
            run: run.to_owned(),
            interrupt: interrupt.clone(),
        })
    }

    /// Await before any user work reaches a harness, mailbox or inference route.
    pub(crate) async fn protect(&self) -> Result<(), String> {
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

    /// Synchronous commit under the session/steer ledger lock closes the Done race.
    pub(crate) fn start_turn(
        &self,
        run: &str,
        interrupt: &CancellationToken,
        cancel: &watch::Sender<bool>,
        events: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<(), String> {
        if self.shared.stop.is_cancelled() {
            return Err("Crew is shutting down".into());
        }
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
        if interrupt.is_cancelled() {
            return Err("Crew turn was interrupted before admission".into());
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

    pub(crate) fn finish_turn(&self, run: &str) {
        finish_turn(&self.shared, run);
    }

    pub(crate) fn failed(&self, run: &str) -> bool {
        self.shared
            .state
            .lock()
            .turns
            .get(run)
            .is_some_and(|turn| turn.failed)
    }

    pub(crate) fn required(&self) -> bool {
        !matches!(&self.shared.binding, Ok(None))
    }

    pub(crate) fn active(&self, run: &str) -> bool {
        let state = self.shared.state.lock();
        !self.shared.stop.is_cancelled()
            && state
                .deadline
                .is_some_and(|at| at > Instant::now() + SAFETY)
            && state
                .turns
                .get(run)
                .is_some_and(|turn| !turn.failed && !turn.interrupt.is_cancelled())
    }

    pub(crate) fn shutdown_now(&self) {
        // No network I/O in Drop; the provider TTL is finite even after a crash.
        self.shared.stop.cancel();
        for turn in self.shared.state.lock().turns.values() {
            let _ = turn.cancel.send(true);
            turn.interrupt.cancel();
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.shutdown_now();
        let worker = self.worker.lock().take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }
        let _operation = self.shared.operation.lock().await;
        let reset = {
            let mut state = self.shared.state.lock();
            if !state.turns.is_empty() {
                state.turns.clear();
                state.refresh = None;
                state.idle_reset = true;
            }
            std::mem::take(&mut state.idle_reset)
        };
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

fn finish_turn(shared: &Shared, run: &str) {
    let mut state = shared.state.lock();
    if state.turns.remove(run).is_some() && state.turns.is_empty() {
        state.refresh = None;
        state.idle_reset = true;
        shared.changed.notify_one();
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
        // Engine cancellation must precede the harness waking/closing its stream.
        let _ = turn.cancel.send(true);
        turn.interrupt.cancel();
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
    let deadline = {
        let mut state = shared.state.lock();
        if !state.turns.is_empty()
            && state
                .deadline
                .is_some_and(|at| at <= Instant::now() + SAFETY)
        {
            fail_active(&mut state);
            return Err(FAILURE.into());
        }
        (!state.turns.is_empty())
            .then_some(state.deadline)
            .flatten()
    };
    // The server resets TTL during this call; request start is the conservative bound.
    let started = Instant::now();
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let result = tokio::select! {
        result = (shared.extend)(id.clone(), cancel.clone()) => result,
        _ = shared.stop.cancelled() => {
            cancel.cancel();
            return Err("Crew is shutting down".into());
        },
        _ = tokio::time::sleep_until(deadline.unwrap_or(started + TTL) - SAFETY), if deadline.is_some() => {
            cancel.cancel();
            Err("Boat renewal exceeded its protection deadline".into())
        },
    };
    let mut state = shared.state.lock();
    if result.is_ok()
        && !shared.stop.is_cancelled()
        && Instant::now() + SAFETY < started + TTL - SAFETY
    {
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
    async fn overlapping_completion_and_cancel_renew_past_one_hour_then_reset_once() {
        let (lease, mut commands) = lease();
        confirm(&lease, &mut commands).await;
        let (a, _, _) = start(&lease, "a");
        let (b, _, _) = start(&lease, "b");
        start(&lease, "b");
        a.cancel();
        lease.finish_turn("a");
        lease.finish_turn("a");
        for _ in 0..13 {
            tokio::time::advance(REFRESH).await;
            commands.recv().await.unwrap().send(Ok(())).unwrap();
            tokio::task::yield_now().await;
        }
        assert!(!b.is_cancelled());
        lease.finish_turn("b");
        commands.recv().await.unwrap().send(Ok(())).unwrap();
        tokio::task::yield_now().await;
        lease.finish_turn("b");
        tokio::time::advance(TTL * 2).await;
        assert!(commands.try_recv().is_err(), "parked runs must not renew");
        lease.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn initial_failure_rejects_and_renewal_failure_interrupts_recoverably() {
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
    async fn idle_steer_requires_new_confirmation_and_guard_releases_failed_start() {
        let (lease, mut commands) = lease();
        confirm(&lease, &mut commands).await;
        let (token, _, _) = start(&lease, "warm");
        let guard = lease.run_guard("warm", &token);
        lease.finish_turn("warm");
        commands.recv().await.unwrap().send(Ok(())).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(TTL * 2).await;
        assert!(commands.try_recv().is_err());
        assert!(
            lease
                .start_turn(
                    "warm",
                    &token,
                    &watch::channel(false).0,
                    &mpsc::unbounded_channel().0
                )
                .is_err()
        );
        confirm(&lease, &mut commands).await;
        start(&lease, "warm");
        drop(guard);
        assert!(token.is_cancelled());
        commands.recv().await.unwrap().send(Ok(())).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(TTL * 2).await;
        assert!(commands.try_recv().is_err());
        lease.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn missed_deadline_interrupts_instead_of_silently_recovering() {
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
    async fn cancelled_admission_preserves_another_protected_turn() {
        let (lease, mut commands) = lease();
        confirm(&lease, &mut commands).await;
        let (token, cancel, _) = start(&lease, "other");
        let mut protection = Box::pin(lease.protect());
        let reply = tokio::select! {
            reply = commands.recv() => reply.unwrap(),
            _ = &mut protection => panic!("premature admission"),
        };
        drop(protection);
        assert!(reply.send(Ok(())).is_err());
        assert!(!token.is_cancelled() && !*cancel.borrow());
        tokio::time::advance(REFRESH).await;
        commands.recv().await.unwrap().send(Ok(())).unwrap();
        tokio::task::yield_now().await;
        assert!(!token.is_cancelled());
        lease.finish_turn("other");
        commands.recv().await.unwrap().send(Ok(())).unwrap();
        tokio::task::yield_now().await;
        lease.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_admission_and_rejects_restart() {
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
