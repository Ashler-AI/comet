//! Client side: request/stream multiplexing over string frames + the WebSocket dialer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::{ClientFrame, RpcError, ServerFrame};

/// A stalled stream fails explicitly after this many queued frames. Its ordered
/// prefix is drained before the error; the connection reader never waits on it.
const STREAM_QUEUE_CAP: usize = 256;

enum Pending {
    Call(oneshot::Sender<Result<serde_json::Value, RpcError>>),
    Stream {
        items: mpsc::Sender<serde_json::Value>,
        terminal: oneshot::Sender<Result<(), RpcError>>,
        cancel: mpsc::OwnedPermit<u64>,
    },
}

struct Shared {
    pending: Mutex<HashMap<u64, Pending>>,
    closed: AtomicBool,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn insert(&self, id: u64, entry: Pending) -> Result<(), RpcError> {
        let mut pending = self.lock();
        if self.closed.load(Ordering::Relaxed) {
            return Err(RpcError::Closed);
        }
        pending.insert(id, entry);
        Ok(())
    }

    fn close(&self) {
        let mut pending = self.lock();
        self.closed.store(true, Ordering::Relaxed);
        for (_, entry) in pending.drain() {
            match entry {
                Pending::Call(tx) => { let _ = tx.send(Err(RpcError::Closed)); }
                Pending::Stream { terminal, .. } => { let _ = terminal.send(Err(RpcError::Closed)); }
            }
        }
    }
}

/// Ordered subscription items, followed by exactly one error on failure or
/// `None` on successful completion. Overflow requires resubscription/replay.
pub struct RpcSubscription {
    items: mpsc::Receiver<serde_json::Value>,
    terminal: oneshot::Receiver<Result<(), RpcError>>,
    ended: bool,
    id: u64,
    shared: Arc<Shared>,
}

impl RpcSubscription {
    pub async fn recv(&mut self) -> Option<Result<serde_json::Value, RpcError>> {
        if self.ended {
            return None;
        }
        if let Some(item) = self.items.recv().await {
            return Some(Ok(item));
        }
        let terminal = (&mut self.terminal).await;
        self.ended = true;
        match terminal.unwrap_or(Err(RpcError::Closed)) {
            Ok(()) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

impl Drop for RpcSubscription {
    fn drop(&mut self) {
        if let Some(Pending::Stream { cancel, .. }) = self.shared.lock().remove(&self.id) {
            cancel.send(self.id);
        }
    }
}

/// A multiplexing RPC client over any string-frame duplex ([`crate::memory_client`] or
/// [`connect_ws`]). Cheap to clone-by-Arc internally; use one per connection.
pub struct RpcClient {
    out: mpsc::Sender<String>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    reader: tokio::task::JoinHandle<()>,
    cancellations: mpsc::Sender<u64>,
    canceller: tokio::task::JoinHandle<()>,
    runtime: tokio::runtime::Handle,
}

impl RpcClient {
    /// Wrap an existing duplex: `out` carries client frames, `inbound` server frames.
    pub fn new(out: mpsc::Sender<String>, mut inbound: mpsc::Receiver<String>) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });
        let reader_shared = shared.clone();
        // Reserve one cancellation slot per subscription before admitting it.
        // One worker handles outbound backpressure; neither reader nor Drop waits.
        let (cancellations, mut cancelled) = mpsc::channel::<u64>(STREAM_QUEUE_CAP);
        let cancel_out = out.clone();
        let canceller = tokio::spawn(async move {
            while let Some(id) = cancelled.recv().await {
                let json = serde_json::to_string(&ClientFrame {
                    id,
                    method: None,
                    params: serde_json::Value::Null,
                    cancel: true,
                }).expect("cancel frame is serializable");
                if cancel_out.send(json).await.is_err() {
                    break;
                }
            }
        });
        let cancel_abort = canceller.abort_handle();
        let reader = tokio::spawn(async move {
            while let Some(payload) = inbound.recv().await {
                for line in payload.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let frame: ServerFrame = match serde_json::from_str(line) {
                        Ok(frame) => frame,
                        Err(err) => {
                            tracing::warn!(error = %err, "rpc: dropping malformed server frame");
                            continue;
                        }
                    };
                    route_frame(&reader_shared, frame);
                }
            }
            reader_shared.close();
            cancel_abort.abort();
        });
        Self {
            out,
            shared,
            next_id: AtomicU64::new(1),
            reader,
            cancellations,
            canceller,
            runtime: tokio::runtime::Handle::current(),
        }
    }

    /// Unary request.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.insert(id, Pending::Call(tx))?;
        self.send(ClientFrame {
            id,
            method: Some(method.into()),
            params,
            cancel: false,
        })
        .await
        .inspect_err(|_| {
            self.shared.lock().remove(&id);
        })?;
        rx.await.map_err(|_| RpcError::Closed)?
    }

    /// A provisioning call whose caller owns cancellation, unlike durable command admission.
    pub async fn call_cancellable(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.insert(id, Pending::Call(tx))?;
        struct CancelOnDrop<'a> {
            client: &'a RpcClient,
            id: u64,
        }
        impl Drop for CancelOnDrop<'_> {
            fn drop(&mut self) {
                if self.client.shared.lock().remove(&self.id).is_some() {
                    if let Ok(frame) = serde_json::to_string(&ClientFrame {
                        id: self.id,
                        method: None,
                        params: serde_json::Value::Null,
                        cancel: true,
                    }) {
                        if let Err(mpsc::error::TrySendError::Full(frame)) =
                            self.client.out.try_send(frame)
                        {
                            let out = self.client.out.clone();
                            self.client.runtime.spawn(async move {
                                let _ = out.send(frame).await;
                            });
                        }
                    }
                }
            }
        }
        let _cancel = CancelOnDrop { client: self, id };
        self.send(ClientFrame {
            id,
            method: Some(method.into()),
            params,
            cancel: false,
        })
        .await?;
        rx.await.map_err(|_| RpcError::Closed)?
    }

    /// Enqueue a unary request without retaining its reply. Intended for
    /// cancellation compensation from `Drop`, where awaiting is impossible
    /// but ordering on the existing RPC channel still matters.
    pub fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let json = serde_json::to_string(&ClientFrame {
            id,
            method: Some(method.into()),
            params,
            cancel: false,
        })
        .map_err(|error| RpcError::Transport(format!("serialize frame: {error}")))?;
        self.out.try_send(json).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => RpcError::Transport("RPC queue is full".into()),
            mpsc::error::TrySendError::Closed(_) => RpcError::Closed,
        })
    }

    /// Typed unary request.
    pub async fn call_as<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T, RpcError> {
        let value = self.call(method, params).await?;
        serde_json::from_value(value).map_err(|e| RpcError::BadParams(e.to_string()))
    }

    /// Streaming request with ordered items and an explicit terminal error.
    /// Dropping cancels immediately, including idle streams. At most 256 active
    /// or queued-for-cancellation streams are admitted (plus one sending cancel).
    pub async fn subscribe(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcSubscription, RpcError> {
        let cancel = self.cancellations.clone().try_reserve_owned().map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => RpcError::Transport("RPC subscription limit reached".into()),
            mpsc::error::TrySendError::Closed(_) => RpcError::Closed,
        })?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, items) = mpsc::channel(STREAM_QUEUE_CAP);
        let (terminal, end) = oneshot::channel();
        self.shared.insert(id, Pending::Stream { items: tx, terminal, cancel })?;
        // Also removes the pending entry if sending the request is cancelled.
        let subscription = RpcSubscription { items, terminal: end, ended: false, id, shared: self.shared.clone() };
        self.send(ClientFrame {
            id,
            method: Some(method.into()),
            params,
            cancel: false,
        })
        .await
        .inspect_err(|_| {
            self.shared.lock().remove(&id);
        })?;
        Ok(subscription)
    }

    async fn send(&self, frame: ClientFrame) -> Result<(), RpcError> {
        let json = serde_json::to_string(&frame)
            .map_err(|e| RpcError::Transport(format!("serialize frame: {e}")))?;
        self.out.send(json).await.map_err(|_| RpcError::Closed)
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.reader.abort();
        self.canceller.abort();
        self.shared.close();
    }
}

fn route_frame(shared: &Arc<Shared>, frame: ServerFrame) {
    let id = frame.id;
    if let Some(err) = frame.err {
        let error = if err == crate::STREAM_OVERFLOW_ERROR {
            RpcError::StreamOverflow
        } else {
            RpcError::Failed(err)
        };
        match shared.lock().remove(&id) {
            Some(Pending::Call(tx)) => { let _ = tx.send(Err(error)); }
            Some(Pending::Stream { terminal, .. }) => { let _ = terminal.send(Err(error)); }
            None => {}
        }
        return;
    }
    if let Some(value) = frame.ok {
        if let Some(Pending::Call(tx)) = shared.lock().remove(&id) {
            let _ = tx.send(Ok(value));
        }
        return;
    }
    if let Some(item) = frame.item {
        let mut pending = shared.lock();
        let error = match pending.get(&id) {
            Some(Pending::Stream { items, .. }) => items.try_send(item).err(),
            _ => None,
        };
        if let Some(error) = error {
            if let Some(Pending::Stream { terminal, cancel, .. }) = pending.remove(&id) {
                let error = match error {
                    mpsc::error::TrySendError::Full(_) => RpcError::StreamOverflow,
                    mpsc::error::TrySendError::Closed(_) => RpcError::Closed,
                };
                let _ = terminal.send(Err(error));
                cancel.send(id);
            }
        }
        return;
    }
    if frame.done {
        if let Some(Pending::Stream { terminal, .. }) = shared.lock().remove(&id) {
            let _ = terminal.send(Ok(()));
        }
    }
}

/// How long a dial may take before we give up.
///
/// This is localhost: a real engine answers in milliseconds. Without a bound,
/// *any* other process holding the port accepts the TCP connection and then
/// never completes the WebSocket handshake, and the caller waits forever — a
/// stranger on port 27654 would hang the app at boot rather than degrade it.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Dial a WebSocket RPC server (`ws://127.0.0.1:{ipc_port}`).
pub async fn connect_ws(url: &str) -> Result<RpcClient, RpcError> {
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| RpcError::Transport(format!("timed out dialing {url}")))?
        .map_err(|e| RpcError::Transport(e.to_string()))?;
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(256);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = out_rx.recv() => match frame {
                    Some(text) => {
                        if sink.send(WsMessage::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = sink.send(WsMessage::Close(None)).await;
                        break;
                    }
                },
                message = stream.next() => match message {
                    Some(Ok(WsMessage::Text(text))) => {
                        if in_tx.send(text).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
            }
        }
    });
    Ok(RpcClient::new(out_tx, in_rx))
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[tokio::test]
    async fn stalled_stream_preserves_prefix_and_does_not_block_other_replies_or_cancel() {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let (out, mut requests) = mpsc::channel(1);
            let (inbound, incoming) = mpsc::channel(1);
            let client = Arc::new(RpcClient::new(out.clone(), incoming));
            let mut stalled = client.subscribe("Transcript", serde_json::Value::Null).await.unwrap();
            let first: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
            let mut other = client.subscribe("Other", serde_json::Value::Null).await.unwrap();
            let second: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
            let caller = client.clone();
            let unary = tokio::spawn(async move { caller.call("Echo", serde_json::Value::Null).await });
            let third: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
            // Keep cancel outbound backpressured while the reader routes every response.
            out.send("blocked outbound".into()).await.unwrap();
            let mut batch = String::new();
            for n in 0..=STREAM_QUEUE_CAP {
                batch.push_str(&serde_json::to_string(&ServerFrame {
                    id: first.id, item: Some(serde_json::json!(n)), ..Default::default()
                }).unwrap());
                batch.push('\n');
            }
            for frame in [
                ServerFrame { id: first.id, done: true, ..Default::default() },
                ServerFrame { id: second.id, item: Some(serde_json::json!("other")), ..Default::default() },
                ServerFrame { id: second.id, done: true, ..Default::default() },
                ServerFrame { id: third.id, ok: Some(serde_json::json!("reply")), ..Default::default() },
            ] {
                batch.push_str(&serde_json::to_string(&frame).unwrap());
                batch.push('\n');
            }
            inbound.send(batch).await.unwrap();
            assert_eq!(unary.await.unwrap().unwrap(), serde_json::json!("reply"));
            assert_eq!(other.recv().await.unwrap().unwrap(), serde_json::json!("other"));
            assert!(other.recv().await.is_none());
            assert_eq!(stalled.items.len(), STREAM_QUEUE_CAP);
            for n in 0..STREAM_QUEUE_CAP {
                assert_eq!(stalled.recv().await.unwrap().unwrap(), serde_json::json!(n));
            }
            assert!(matches!(stalled.recv().await, Some(Err(RpcError::StreamOverflow))));
            assert!(stalled.recv().await.is_none());
            assert!(client.shared.lock().is_empty());
            assert_eq!(requests.recv().await.as_deref(), Some("blocked outbound"));
            let cancel: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
            assert!(cancel.cancel);
            assert_eq!(cancel.id, first.id);
        }).await.expect("a stalled stream must not stall the shared reader");
    }

    #[tokio::test]
    async fn stream_terminal_and_connection_close_follow_the_queued_prefix() {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
        for end in ["done", "error", "close", "drop"] {
            let (out, mut requests) = mpsc::channel(1);
            let (_inbound, incoming) = mpsc::channel(1);
            let client = RpcClient::new(out, incoming);
            let mut stream = client.subscribe("Count", serde_json::Value::Null).await.unwrap();
            let request: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
            for n in 0..STREAM_QUEUE_CAP {
                route_frame(&client.shared, ServerFrame {
                    id: request.id, item: Some(serde_json::json!(n)), ..Default::default()
                });
            }
            match end {
                "done" => route_frame(&client.shared, ServerFrame { id: request.id, done: true, ..Default::default() }),
                "error" => route_frame(&client.shared, ServerFrame { id: request.id, err: Some("failed".into()), ..Default::default() }),
                "close" => {
                    // A call admitted before closure must also settle.
                    let (tx, rx) = oneshot::channel();
                    client.shared.insert(1000, Pending::Call(tx)).unwrap();
                    drop(_inbound);
                    assert!(matches!(rx.await.unwrap(), Err(RpcError::Closed)));
                    assert!(matches!(client.subscribe("Later", serde_json::Value::Null).await, Err(RpcError::Closed)));
                }
                "drop" => drop(client),
                _ => unreachable!(),
            }
            for n in 0..STREAM_QUEUE_CAP {
                assert_eq!(stream.recv().await.unwrap().unwrap(), serde_json::json!(n));
            }
            match end {
                "done" => assert!(stream.recv().await.is_none()),
                "error" => assert!(matches!(stream.recv().await, Some(Err(RpcError::Failed(error))) if error == "failed")),
                _ => assert!(matches!(stream.recv().await, Some(Err(RpcError::Closed)))),
            }
            assert!(stream.recv().await.is_none());
        }
        }).await.expect("terminal and disconnect must settle after draining the prefix");
    }

    #[tokio::test]
    async fn idle_drop_cancels_and_subscription_admission_bounds_cancel_resources() {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let (out, mut requests) = mpsc::channel(1);
            let (_inbound, incoming) = mpsc::channel(1);
            let client = RpcClient::new(out, incoming);
            let mut subscriptions = Vec::new();
            for _ in 0..STREAM_QUEUE_CAP {
                subscriptions.push(client.subscribe("Idle", serde_json::Value::Null).await.unwrap());
                requests.recv().await.unwrap();
            }
            assert!(matches!(client.subscribe("TooMany", serde_json::Value::Null).await, Err(RpcError::Transport(_))));
            drop(subscriptions);
            assert!(client.shared.lock().is_empty());
            let mut cancelled_ids = Vec::new();
            for _ in 0..STREAM_QUEUE_CAP {
                let cancel: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
                assert!(cancel.cancel);
                cancelled_ids.push(cancel.id);
            }
            cancelled_ids.sort_unstable();
            assert_eq!(cancelled_ids, (1..=STREAM_QUEUE_CAP as u64).collect::<Vec<_>>());
            let _again = client.subscribe("Available", serde_json::Value::Null).await.unwrap();
        }).await.expect("idle streams must cancel without waiting for another item");
    }

    #[tokio::test]
    async fn dropping_durable_call_does_not_cancel_command_admission() {
        let (out, mut requests) = mpsc::channel(1);
        let (_inbound, incoming) = mpsc::channel(1);
        let client = Arc::new(RpcClient::new(out, incoming));
        let caller = client.clone();
        let command = tokio::spawn(async move { caller.call("QueueCommand", serde_json::Value::Null).await });
        let request: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
        command.abort();
        let _ = command.await;
        assert!(client.shared.lock().contains_key(&request.id));
        assert!(matches!(requests.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        route_frame(&client.shared, ServerFrame {
            id: request.id, ok: Some(serde_json::json!({"admitted": true})), ..Default::default()
        });
        assert!(client.shared.lock().is_empty());
    }

    #[tokio::test]
    async fn cancelled_preparation_delivers_cancel_even_when_outbound_queue_is_full() {
        let (out, mut frames) = mpsc::channel(1);
        let (_inbound, incoming) = mpsc::channel(1);
        let client = Arc::new(RpcClient::new(out.clone(), incoming));
        let pending_client = client.clone();
        let task = tokio::spawn(async move {
            pending_client
                .call_cancellable("PrepareScaffoldSession", serde_json::json!({}))
                .await
        });
        let request: ClientFrame = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
        out.send("queued".into()).await.unwrap();
        task.abort();
        let _ = task.await;
        assert_eq!(frames.recv().await.as_deref(), Some("queued"));
        let cancel: ClientFrame = serde_json::from_str(
            &tokio::time::timeout(std::time::Duration::from_secs(1), frames.recv())
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(cancel.cancel);
        assert_eq!(cancel.id, request.id);
        assert!(client.shared.lock().is_empty());
    }
}
