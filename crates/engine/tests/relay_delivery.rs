//! Peer-relay delivery fallback (durable-by-design §Phase 3): engine A queues
//! a command for a chat hosted on engine B, A's chat2 rows can NEVER reach an
//! edge (none is configured — the 03:45 incident shape, rows dark while the
//! peer link lives), so after the rows grace A relay-forwards the entry over
//! the device-room link. B claims the client-minted id in its processed
//! ledger before executing — so when the doc row later "arrives" (simulated
//! by writing the same entry into B's doc), the drain dedupes it to a no-op:
//! exactly-once across both roads.

// tungstenite's `accept_hdr_async` callback signature fixes the Err type as a
// full `Response` — its size is not ours to shrink.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as WsRequest, Response as WsResponse,
};

use zeron_doc::{MessageRole, MessageStatus, SessionCommandPayload};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, Device, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};
use zeron_rpc::{
    DeviceFrameHeader, HostRelay, HostRelayConfig, LinkCache, LinkCacheConfig, RpcError, RpcReply,
    RpcService, StaticToken, decode_device_frame, encode_device_frame, methods,
};

const CHAT: &str = "chat-relay-fallback";

// Minimal in-memory device room (route-only subset of the DO semantics) —
// same shape as device_routing.rs.
#[derive(Default)]
struct RelayState {
    host: Option<mpsc::UnboundedSender<Vec<u8>>>,
    clients: HashMap<String, mpsc::UnboundedSender<Vec<u8>>>,
}

async fn fake_device_room() -> (String, tokio::task::JoinHandle<()>) {
    observed_device_room(None).await
}

async fn observed_device_room(
    refusals: Option<mpsc::UnboundedSender<()>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
    let url = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let state = Arc::new(Mutex::new(HashMap::<String, RelayState>::new()));
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let state = state.clone();
            let refusals = refusals.clone();
            tokio::spawn(async move {
                let mut uri = String::new();
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |req: &WsRequest, res: WsResponse| {
                        uri = req.uri().to_string();
                        Ok(res)
                    },
                )
                .await
                else {
                    return;
                };
                let query = uri.split_once('?').map(|(_, q)| q).unwrap_or("");
                let room = uri.split('?').next().expect("device room path").to_owned();
                let is_host = query.contains("role=host");
                let conn_id = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("connId="))
                    .unwrap_or("anon")
                    .to_string();
                let (mut sink, mut ws_stream) = ws.split();
                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                {
                    let mut rooms = state.lock().expect("lock");
                    let st = rooms.entry(room.clone()).or_default();
                    if is_host {
                        st.host = Some(tx);
                    } else {
                        st.clients.insert(conn_id.clone(), tx);
                    }
                }
                let writer = tokio::spawn(async move {
                    while let Some(bytes) = rx.recv().await {
                        if sink.send(WsMessage::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                });
                while let Some(Ok(message)) = ws_stream.next().await {
                    let WsMessage::Binary(bytes) = message else {
                        continue;
                    };
                    let Ok((header, payload)) = decode_device_frame(&bytes) else {
                        break;
                    };
                    let rooms = state.lock().expect("lock");
                    let Some(st) = rooms.get(&room) else { continue };
                    if is_host {
                        let Some(to) = header.to else { continue };
                        if let Some(client) = st.clients.get(&to) {
                            let stripped = DeviceFrameHeader::new(header.s, header.k);
                            let _ = client
                                .send(encode_device_frame(&stripped, &payload).expect("encode"));
                            if let Some(refusals) = &refusals
                                && serde_json::from_slice::<serde_json::Value>(&payload)
                                    .ok().is_some_and(|frame| frame["err"].as_str()
                                        == Some("injected receiver refusal after staging"))
                            {
                                let _ = refusals.send(());
                            }
                        }
                    } else if let Some(host) = &st.host {
                        let mut routed = DeviceFrameHeader::new(header.s, header.k);
                        routed.from = Some(conn_id.clone());
                        let _ = host.send(encode_device_frame(&routed, &payload).expect("encode"));
                    }
                }
                writer.abort();
            });
        }
    });
    (url, task)
}

struct InstantHarness(Arc<Mutex<Vec<RunRequest>>>);

#[async_trait]
impl Harness for InstantHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Instant"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let cwd = request.cwd.clone();
        self.0.lock().expect("record harness request").push(request);
        Ok(futures::stream::iter([
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "instant-1".into(),
                tools: vec![],
                cwd,
                session_id: "hs-relay".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: "relayed reply".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-relay".into()),
            }),
        ])
        .boxed())
    }
}

fn registry(requests: Arc<Mutex<Vec<RunRequest>>>) -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(InstantHarness(requests)));
    Arc::new(registry)
}

fn assemble(
    dir: &std::path::Path,
    device_id: &str,
    requests: Arc<Mutex<Vec<RunRequest>>>,
) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    let core = EngineCore::assemble(dir, registry(requests), HarnessId::Mock, None)
        .expect("engine assembles");
    // The fake room still needs an explicit token source; production auth
    // correctly stays signed out when no credentials have been configured.
    let mut auth = zeron_engine::AuthConfig::new("http://localhost:27640", dir);
    auth.allow_dev_auth = true;
    core.set_auth(zeron_engine::Auth::new(auth));
    core
}

fn complete_assistant_count(core: &EngineCore) -> usize {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
        .iter()
        .filter(|e| e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_dark_command_delivers_over_the_peer_relay_exactly_once() {
    let (relay_url, _relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("tempdir");
    let runs_a = Arc::new(Mutex::new(Vec::new()));
    let runs_b = Arc::new(Mutex::new(Vec::new()));
    let cwd_b = dirs.path().join("project-on-b");
    std::fs::create_dir(&cwd_b).expect("B project directory");

    // Engine B hosts its device room on the fake relay.
    let core_b = assemble(&dirs.path().join("b"), "device-b", runs_b.clone());
    let _host = core_b.start_host_relay(&relay_url);

    // Engine A dials peers through the same relay — and has NO edge, so its
    // chat2 rows can never flush (the rows-dark half of the incident shape).
    let core_a = assemble(&dirs.path().join("a"), "device-a", runs_a.clone());
    let mut link_config =
        LinkCacheConfig::new(relay_url.clone(), Arc::new(StaticToken("test-user".into())));
    link_config.probe_timeout = Duration::from_secs(5);
    core_a.set_links(LinkCache::new(link_config));

    // A knows the chat is hosted on B (local registry writes), and knows B's
    // stamped version passes the relay gate.
    core_a.workspace.upsert_device_row(&Device {
        id: "device-b".into(),
        name: "b".into(),
        platform: "linux".into(),
        devbox_provider: None,
        devbox_id: None,
        last_seen_at: Some(chrono::Utc::now()),
        created_at: None,
        version: Some("0.2.12".into()),
        cursor_sdk_version: None,
        capabilities: zeron_proto::capabilities::current(),
    });
    let client_a = zeron_rpc::memory_client(core_a.rpc_service());
    client_a
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": CHAT, "deviceId": "device-b" }),
        )
        .await
        .expect("createChat on A");
    core_b
        .workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("pre-title on B (no auto-title harness run)");

    // The send: a durable local write on A. Rows go nowhere; the escort's
    // grace elapses; the entry crosses the peer link instead.
    let command = serde_json::to_value(SessionCommandPayload::Run {
        request: RunRequest {
            mcp: None,
            prompt: "over the relay".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: cwd_b.to_str().expect("UTF-8 B cwd").into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            attachments: Vec::new(),
            worktree: None,
            resume: None,
        },
        message_id: "msg-relay-1".into(),
    })
    .expect("command json");
    client_a
        .call(
            methods::QUEUE_COMMAND,
            serde_json::json!({ "chatId": CHAT, "command": command }),
        )
        .await
        .expect("queue on A");

    // B executes it — allow the 10s rows grace plus relay dial time.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if complete_assistant_count(&core_b) == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "relayed command never executed on B"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let entries = core_b
        .doc_host
        .open(CHAT)
        .expect("open on B")
        .doc()
        .read_entries()
        .expect("read entries");
    assert!(
        entries
            .iter()
            .any(|e| e.id == "msg-relay-1" && e.role == MessageRole::User),
        "B persisted the user message under the client-minted id"
    );
    assert!(runs_a.lock().expect("A requests").is_empty(), "A must not execute");
    {
        let requests = runs_b.lock().expect("B requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(std::path::Path::new(&requests[0].cwd), cwd_b);
    }

    // Exactly-once: the doc row "arrives" later over chat2 sync — simulate by
    // writing A's exact entry into B's doc, which kicks B's drain. The
    // processed ledger must dedupe it to a no-op.
    let entry = core_a
        .doc_host
        .open(CHAT)
        .expect("open on A")
        .doc()
        .read_commands()
        .expect("read A's commands")
        .into_iter()
        .next()
        .expect("A queued exactly one command");
    let handle_b = core_b.doc_host.open(CHAT).expect("open on B");
    handle_b
        .doc()
        .queue_command(&entry)
        .expect("simulate the synced doc row");
    // A second relay attempt (a retrying sender) must also dedupe.
    let dup = core_b
        .doc_host
        .ingest_relayed_command(CHAT, entry)
        .await
        .expect("duplicate relay accepted");
    assert_eq!(dup, "duplicate");
    // The duplicate relay acquires the same prompt lock as the doc drain;
    // explicitly drain again instead of guessing when its watcher ran.
    core_b.doc_host.drain_commands(&handle_b).await;
    assert_eq!(runs_b.lock().expect("B requests after replay").len(), 1);
    assert!(runs_a.lock().expect("A requests after replay").is_empty());
    assert_eq!(
        complete_assistant_count(&core_b),
        1,
        "the doc row + a duplicate relay must not double-run the command"
    );

    core_a.shutdown().await;
    core_b.shutdown().await;
}

// These gates delay replies only AFTER the production Uploads handler has
// written the chunk. The sender still uses its queued delivery worker, RPC
// serialization, peer link, relay, and the receiver's real commit assembly.
#[derive(Debug)]
struct ChunkAck {
    upload_id: String,
    seq: u64,
    encoded_len: usize,
    refuse: oneshot::Sender<bool>,
}

#[derive(Debug)]
enum UploadEvent {
    Chunk(ChunkAck),
    Commit(String),
}

#[derive(Default)]
struct UploadWindow {
    active: usize,
    max_active: usize,
    chunks: Vec<(String, u64, usize)>,
    commits: Vec<String>,
    relays: usize,
}

struct ActiveChunk(Arc<Mutex<UploadWindow>>);

impl Drop for ActiveChunk {
    fn drop(&mut self) {
        self.0.lock().expect("upload window").active -= 1;
    }
}

struct GatedUploads {
    inner: Arc<dyn RpcService>,
    events: mpsc::UnboundedSender<UploadEvent>,
    window: Arc<Mutex<UploadWindow>>,
}

#[async_trait]
impl RpcService for GatedUploads {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        if method == methods::UPLOAD_CHUNK {
            let upload_id = params["uploadId"].as_str().expect("upload id").to_owned();
            let seq = params["seq"].as_u64().expect("positional chunk");
            let encoded_len = params["data"].as_str().expect("base64 data").len();
            {
                let mut window = self.window.lock().expect("upload window");
                window.active += 1;
                window.max_active = window.max_active.max(window.active);
                window.chunks.push((upload_id.clone(), seq, encoded_len));
            }
            let _active = ActiveChunk(self.window.clone());
            let reply = self.inner.handle(method, params).await?;
            let (refuse, release) = oneshot::channel();
            self.events.send(UploadEvent::Chunk(ChunkAck {
                upload_id,
                seq,
                encoded_len,
                refuse,
            })).expect("test receives chunk ACK gate");
            if release.await.map_err(|_| RpcError::Failed("test dropped ACK gate".into()))? {
                return Err(RpcError::Failed("injected receiver refusal after staging".into()));
            }
            return Ok(reply);
        }
        if method == methods::UPLOAD_COMMIT {
            let upload_id = params["uploadId"].as_str().expect("commit upload id").to_owned();
            self.window.lock().expect("upload window").commits.push(upload_id.clone());
            self.events.send(UploadEvent::Commit(upload_id)).expect("test receives commit");
        }
        if method == methods::RELAY_COMMAND {
            self.window.lock().expect("upload window").relays += 1;
        }
        self.inner.handle(method, params).await
    }
}

fn gated_upload_host(
    core: &EngineCore,
    relay_url: &str,
) -> (HostRelay, mpsc::UnboundedReceiver<UploadEvent>, Arc<Mutex<UploadWindow>>) {
    let (events, received) = mpsc::unbounded_channel();
    let window = Arc::new(Mutex::new(UploadWindow::default()));
    let service = Arc::new(GatedUploads {
        inner: core.rpc_service(),
        events,
        window: window.clone(),
    });
    let host = core.doc_host.clone();
    let relay = HostRelay::spawn(
        HostRelayConfig::new(relay_url, core.workspace.device_id(), Arc::new(StaticToken("test-user".into()))),
        service,
        Arc::new(move |chat_id| host.enqueue_wakeup(&chat_id).is_ok()),
    );
    (relay, received, window)
}

fn attachment_links(core: &EngineCore, relay_url: &str) {
    let mut config = LinkCacheConfig::new(
        relay_url.to_owned(), Arc::new(StaticToken("test-user".into())),
    );
    config.probe_timeout = Duration::from_secs(5);
    core.set_links(LinkCache::new(config));
}

async fn create_attachment_chat(core: &EngineCore) {
    core.workspace.upsert_device_row(&Device {
        id: "device-b".into(),
        name: "b".into(),
        platform: "linux".into(),
        devbox_provider: None,
        devbox_id: None,
        last_seen_at: Some(chrono::Utc::now()),
        created_at: None,
        version: Some("0.2.12".into()),
        cursor_sdk_version: None,
        capabilities: zeron_proto::capabilities::current(),
    });
    zeron_rpc::memory_client(core.rpc_service()).call(
        methods::MUTATE,
        serde_json::json!({ "op": "createChat", "chatId": CHAT, "deviceId": "device-b" }),
    ).await.expect("create remote attachment chat");
}

fn attachment_bytes(len: usize) -> Vec<u8> {
    // Non-repeating chunk boundaries expose reordering, duplication and stale
    // tails; include every byte value, not just base64-friendly ASCII.
    (0..len).map(|i| ((i * 31 + i / 997) % 256) as u8).collect()
}

fn stage_attachment(core: &EngineCore, upload_id: &str, name: &str, bytes: &[u8]) {
    core.uploads.append(upload_id, &BASE64.encode(bytes), Some(0)).expect("stage source");
    core.uploads.commit(upload_id, name).expect("commit source locally");
}

async fn queue_attachment(core: &EngineCore, upload_id: &str, name: &str) {
    let command = SessionCommandPayload::Run {
        request: RunRequest {
            mcp: None,
            prompt: "deliver the queued attachment".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: core.uploads.dir().to_string_lossy().into_owned(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            attachments: vec![zeron_engine::uploads::pending_ref(upload_id, name)],
            worktree: None,
            resume: None,
        },
        message_id: format!("queued-attachment-{upload_id}"),
    };
    zeron_rpc::memory_client(core.rpc_service()).call(
        methods::QUEUE_COMMAND,
        serde_json::json!({
            "chatId": CHAT,
            "command": command,
            "transfers": [{ "uploadId": upload_id, "fileName": name }],
        }),
    ).await.expect("queue attachment through production RPC");
}

async fn upload_event(events: &mut mpsc::UnboundedReceiver<UploadEvent>) -> UploadEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await.expect("queued uploader made no progress")
        .expect("upload service stopped")
}

async fn chunk_ack(events: &mut mpsc::UnboundedReceiver<UploadEvent>) -> ChunkAck {
    match upload_event(events).await {
        UploadEvent::Chunk(ack) => ack,
        UploadEvent::Commit(id) => panic!("commit crossed an unacknowledged window: {id}"),
    }
}

async fn first_window(events: &mut mpsc::UnboundedReceiver<UploadEvent>) -> HashMap<u64, ChunkAck> {
    let mut held = HashMap::new();
    for _ in 0..3 {
        let ack = chunk_ack(events).await;
        assert!(held.insert(ack.seq, ack).is_none(), "duplicate chunk in initial window");
    }
    assert!(held.contains_key(&0) && held.contains_key(&1) && held.contains_key(&2));
    held
}

// Wait for the real error response to cross the relay, then complete a
// round-trip on that SAME cached RPC client. Its inbound FIFO has therefore
// delivered the refusal before any sibling ACK is released. No sleeps or
// scheduler-order assumptions are needed to test the failed window.
async fn refuse_and_flush(
    core: &EngineCore,
    held: &mut HashMap<u64, ChunkAck>,
    refusals: &mut mpsc::UnboundedReceiver<()>,
) {
    release_ack(held, 0, true);
    tokio::time::timeout(Duration::from_secs(10), refusals.recv())
        .await.expect("refusal did not cross relay").expect("relay observer open");
    let peer = core.dial_device("device-b").await.expect("same cached peer client");
    peer.call(methods::LIST_HARNESSES, serde_json::json!({}))
        .await.expect("flush inbound RPC responses before releasing sibling ACKs");
}

fn release_ack(held: &mut HashMap<u64, ChunkAck>, seq: u64, refuse: bool) {
    held.remove(&seq).expect("held ACK").refuse.send(refuse)
        .expect("uploader cancelled an unsettled request");
}

async fn transfer_progress(core: &EngineCore, upload_id: &str, done: Option<u64>) {
    let mut progress = core.doc_host.watch_transfers();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let matched = {
                let snapshot = progress.borrow_and_update();
                let transfer = snapshot.iter().find(|t| t.upload_id == upload_id);
                match done {
                    Some(done) => transfer.is_some_and(|t| t.done == done),
                    None => transfer.is_none(),
                }
            };
            if matched { return; }
            progress.changed().await.expect("transfer progress stays open");
        }
    }).await.expect("uploader progress did not reach expected state");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_upload_pipelines_three_out_of_order_acks_and_waits_for_final_ack_before_commit() {
    let (relay_url, relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("attachment test directories");
    let core_b = assemble(&dirs.path().join("b"), "device-b", Arc::default());
    let (host, mut events, window) = gated_upload_host(&core_b, &relay_url);
    let core_a = assemble(&dirs.path().join("a"), "device-a", Arc::default());
    attachment_links(&core_a, &relay_url);
    create_attachment_chat(&core_a).await;
    let upload_id = "pipeline-upload";
    let wire_id = format!("{upload_id}-q680000");
    let name = "window bytes.bin";
    let bytes = attachment_bytes(4 * 510_000 + 17);
    stage_attachment(&core_a, upload_id, name, &bytes);
    queue_attachment(&core_a, upload_id, name).await;

    let mut held = first_window(&mut events).await;
    for ack in held.values() {
        assert_eq!(ack.upload_id, wire_id, "wire staging must isolate the logical identity");
        assert_eq!(ack.encoded_len, 680_000);
    }
    assert_eq!(window.lock().expect("window").active, 3);
    // A UI retry while the owner is still live must coalesce, not start a
    // second three-request window for this same durable command/identity.
    core_a.doc_host.retry_delivery(CHAT).expect("retry while initial ACKs are held");
    // ACK 2 refills the window while 0 and 1 are still held: this fails for a
    // serial sender and for an ordered join that head-of-line blocks on 0.
    release_ack(&mut held, 2, false);
    let fourth = chunk_ack(&mut events).await;
    assert_eq!(fourth.seq, 3);
    assert_eq!(fourth.encoded_len, 680_000);
    held.insert(fourth.seq, fourth);
    release_ack(&mut held, 0, false);
    let final_ack = chunk_ack(&mut events).await;
    assert_eq!(final_ack.seq, 4);
    assert_eq!(final_ack.encoded_len, 24);
    held.insert(final_ack.seq, final_ack);
    release_ack(&mut held, 1, false);
    release_ack(&mut held, 3, false);
    transfer_progress(&core_a, upload_id, Some(4 * 510_000)).await;

    // Every byte is already staged at B, including the tail, but its final
    // ACK is not released. Commit must be gated by ACKs, not dispatch/write.
    assert!(!core_b.uploads.pending_target(upload_id, name).exists());
    assert!(window.lock().expect("window").commits.is_empty());
    assert!(events.try_recv().is_err(), "no commit while final ACK is held");
    let progress = core_a.doc_host.watch_transfers();
    assert_eq!(progress.borrow()[0].total, bytes.len() as u64);
    release_ack(&mut held, 4, false);
    match upload_event(&mut events).await {
        UploadEvent::Commit(id) => assert_eq!(id, wire_id),
        UploadEvent::Chunk(ack) => panic!("unexpected extra chunk {}", ack.seq),
    }
    transfer_progress(&core_a, upload_id, None).await;
    assert_eq!(std::fs::read(core_b.uploads.pending_target(upload_id, name)).unwrap(), bytes);
    {
        let window = window.lock().expect("window");
        assert_eq!(window.max_active, 3, "bounded concurrent RPC window");
        assert_eq!(window.active, 0);
        assert_eq!(window.chunks.len(), 5);
        assert!(window.chunks.iter().all(|(id, _, _)| id == &wire_id));
        assert_eq!(window.commits, [wire_id]);
    }
    core_a.shutdown().await;
    core_b.shutdown().await;
    drop(host);
    relay.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_upload_isolates_old_writer_partials_across_reopen_and_retry() {
    let (refusal_tx, mut refusals) = mpsc::unbounded_channel();
    let (relay_url, relay) = observed_device_room(Some(refusal_tx)).await;
    let dirs = tempfile::tempdir().expect("attachment test directories");
    let a_dir = dirs.path().join("a");
    let b_dir = dirs.path().join("b");
    let core_b = assemble(&b_dir, "device-b", Arc::default());
    let upload_id = "24b7a1d2-725c-4ec3-b75b-ae0db2140bd9";
    let wire_id = format!("{upload_id}-q680000");
    let name = "mixed writer bytes.bin";
    let bytes = attachment_bytes(5_357_034);
    let old_first = BASE64.encode(&bytes[..45_000]);
    let old_high = BASE64.encode(&bytes[11 * 45_000..12 * 45_000]);
    // An old peer stages raw logical IDs in 60k slots. Slot 11 would append
    // exactly 45,000 unwanted bytes after this file's eleven fast chunks.
    let receiver = zeron_rpc::memory_client(core_b.rpc_service());
    for (seq, data) in [(0, &old_first), (11, &old_high)] {
        receiver.call(methods::UPLOAD_CHUNK, serde_json::json!({
            "uploadId": upload_id, "seq": seq, "data": data,
        })).await.expect("seed old-writer partial through real RPC");
    }
    drop(receiver);
    let (host, mut events, window) = gated_upload_host(&core_b, &relay_url);
    let core_a = assemble(&a_dir, "device-a", Arc::default());
    attachment_links(&core_a, &relay_url);
    create_attachment_chat(&core_a).await;
    stage_attachment(&core_a, upload_id, name, &bytes);
    queue_attachment(&core_a, upload_id, name).await;
    let mut held = first_window(&mut events).await;
    assert!(held.values().all(|ack| ack.upload_id == wire_id && ack.encoded_len == 680_000));
    refuse_and_flush(&core_a, &mut held, &mut refusals).await;
    release_ack(&mut held, 1, false);
    transfer_progress(&core_a, upload_id, Some(510_000)).await;
    release_ack(&mut held, 2, false);
    transfer_progress(&core_a, upload_id, None).await;
    {
        let window = window.lock().expect("window");
        assert_eq!(window.chunks.len(), 3, "refusal must stop window refills");
        assert!(window.commits.is_empty());
        assert_eq!(window.active, 0);
    }
    assert!(!core_b.uploads.pending_target(upload_id, name).exists());
    core_a.shutdown().await;
    core_b.shutdown().await;
    drop(host);
    drop(core_a);
    drop(core_b);

    // Reopen both stores: retry reconstructs the unchanged logical ID from
    // the durable pending ref, then selects the SAME fast wire namespace.
    let core_b = assemble(&b_dir, "device-b", Arc::default());
    let old_dir = core_b.uploads.dir().join("tmp").join(upload_id);
    assert_eq!(std::fs::read_to_string(old_dir.join("000000.b64")).unwrap(), old_first);
    assert_eq!(std::fs::read_to_string(old_dir.join("000011.b64")).unwrap(), old_high);
    assert!(core_b.uploads.dir().join("tmp").join(&wire_id).join("000002.b64").is_file());
    let (host, mut events, window) = gated_upload_host(&core_b, &relay_url);
    let core_a = assemble(&a_dir, "device-a", Arc::default());
    attachment_links(&core_a, &relay_url);
    assert_eq!(std::fs::read(core_a.uploads.pending_target(upload_id, name)).unwrap(), bytes);
    zeron_rpc::memory_client(core_a.rpc_service()).call(
        methods::RETRY_DELIVERY, serde_json::json!({ "chatId": CHAT }),
    ).await.expect("retry the durable pending command after restart");
    let mut held = first_window(&mut events).await;
    assert!(held.values().all(|ack| ack.upload_id == wire_id && ack.encoded_len == 680_000));
    // A different, old sender can still write the original ID during retry.
    // Its positional writes must neither overwrite fast chunks nor get lost.
    let receiver = zeron_rpc::memory_client(core_b.rpc_service());
    for (seq, data) in [(0, &old_first), (11, &old_high)] {
        receiver.call(methods::UPLOAD_CHUNK, serde_json::json!({
            "uploadId": upload_id, "seq": seq, "data": data,
        })).await.expect("old peer writes while fast retry is active");
    }
    let mut seen = std::collections::HashSet::new();
    for seq in [2, 0, 1] {
        seen.insert(seq);
        release_ack(&mut held, seq, false);
    }
    for _ in 3..11 {
        let ack = chunk_ack(&mut events).await;
        assert_eq!(ack.upload_id, wire_id);
        assert!((3..11).contains(&ack.seq));
        assert_eq!(ack.encoded_len, if ack.seq == 10 { 342_712 } else { 680_000 });
        assert!(seen.insert(ack.seq), "no duplicated retry slots");
        ack.refuse.send(false).expect("retry consumes every ACK");
    }
    match upload_event(&mut events).await {
        UploadEvent::Commit(id) => assert_eq!(id, wire_id),
        UploadEvent::Chunk(ack) => panic!("unexpected retry chunk {}", ack.seq),
    }
    transfer_progress(&core_a, upload_id, None).await;
    let logical_ref = zeron_engine::uploads::pending_ref(upload_id, name);
    let resolved = core_b.uploads.resolve_pending(&logical_ref).expect("unchanged logical ref resolves");
    let delivered = std::fs::read(resolved).unwrap();
    assert_eq!(delivered.len(), 5_357_034, "no extra 45,000-byte legacy tail");
    assert_eq!(delivered, bytes);
    assert_eq!(std::fs::read_to_string(old_dir.join("000000.b64")).unwrap(), old_first);
    assert_eq!(std::fs::read_to_string(old_dir.join("000011.b64")).unwrap(), old_high);
    assert!(!core_b.uploads.dir().join("tmp").join(&wire_id).exists());
    {
        let window = window.lock().expect("window");
        assert_eq!(window.max_active, 3);
        assert_eq!(window.active, 0);
        assert_eq!(window.chunks.len(), 11);
        assert!(window.chunks.iter().all(|(id, _, _)| id == &wire_id));
        assert_eq!(window.commits, [wire_id]);
    }
    core_a.shutdown().await;
    core_b.shutdown().await;
    drop(host);
    relay.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_upload_refusal_settles_outstanding_window_before_clearing_progress() {
    let (refusal_tx, mut refusals) = mpsc::unbounded_channel();
    let (relay_url, relay) = observed_device_room(Some(refusal_tx)).await;
    let dirs = tempfile::tempdir().expect("attachment test directories");
    let core_b = assemble(&dirs.path().join("b"), "device-b", Arc::default());
    let (host, mut events, window) = gated_upload_host(&core_b, &relay_url);
    let core_a = assemble(&dirs.path().join("a"), "device-a", Arc::default());
    attachment_links(&core_a, &relay_url);
    create_attachment_chat(&core_a).await;
    let upload_id = "failurex-upload";
    let name = "failed window.bin";
    let bytes = attachment_bytes(4 * 510_000 + 17);
    stage_attachment(&core_a, upload_id, name, &bytes);
    queue_attachment(&core_a, upload_id, name).await;
    let mut held = first_window(&mut events).await;
    refuse_and_flush(&core_a, &mut held, &mut refusals).await;
    release_ack(&mut held, 1, false);
    // A successful sibling ACK must still be consumed after the error. Its
    // held sibling remains live, so the progress guard cannot retire yet.
    transfer_progress(&core_a, upload_id, Some(510_000)).await;
    assert_eq!(window.lock().expect("window").active, 1);
    assert!(!core_b.uploads.pending_target(upload_id, name).exists());
    release_ack(&mut held, 2, false);
    transfer_progress(&core_a, upload_id, None).await;
    {
        let window = window.lock().expect("window");
        assert_eq!(window.max_active, 3);
        assert_eq!(window.active, 0, "failure leaves no old writes running");
        assert_eq!(window.chunks.len(), 3, "error stops new requests");
        assert!(window.commits.is_empty(), "failed attempt must never commit");
    }
    assert!(events.try_recv().is_err());
    assert!(!core_b.uploads.pending_target(upload_id, name).exists());
    assert_eq!(std::fs::read(core_a.uploads.pending_target(upload_id, name)).unwrap(), bytes);
    core_a.shutdown().await;
    core_b.shutdown().await;
    drop(host);
    relay.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_upload_empty_tails_exact_boundaries_and_id_fallbacks_commit_exact_bytes() {
    let (relay_url, relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("attachment test directories");
    let core_b = assemble(&dirs.path().join("b"), "device-b", Arc::default());
    let (host, mut events, window) = gated_upload_host(&core_b, &relay_url);
    let core_a = assemble(&dirs.path().join("a"), "device-a", Arc::default());
    attachment_links(&core_a, &relay_url);
    create_attachment_chat(&core_a).await;
    let name = "boundary.bin";
    for (upload_id, len, chunks, chunk_bytes) in [
        ("empty000".to_owned(), 0, 1, 510_000),
        ("exactone".to_owned(), 510_000, 1, 510_000),
        ("exacttri".to_owned(), 3 * 510_000, 3, 510_000),
        ("onetail0".to_owned(), 510_001, 2, 510_000),
        ("twotail0".to_owned(), 510_002, 2, 510_000),
        ("shortid".to_owned(), 45_001, 2, 45_000),
        ("m".repeat(64), 90_000, 2, 45_000),
        ("f".repeat(56), 510_000, 1, 510_000),
        ("o".repeat(57), 45_000, 1, 45_000),
    ] {
        let upload_id = upload_id.as_str();
        let wire_id = if chunk_bytes == 510_000 { format!("{upload_id}-q680000") } else { upload_id.to_owned() };
        let bytes = attachment_bytes(len);
        stage_attachment(&core_a, upload_id, name, &bytes);
        queue_attachment(&core_a, upload_id, name).await;
        let mut held = HashMap::new();
        for _ in 0..chunks {
            let ack = chunk_ack(&mut events).await;
            assert_eq!(ack.upload_id, wire_id);
            let start = ack.seq as usize * chunk_bytes;
            let raw_len = (len - start).min(chunk_bytes);
            assert_eq!(ack.encoded_len, raw_len.div_ceil(3) * 4);
            assert!(ack.seq < chunks);
            assert!(held.insert(ack.seq, ack).is_none());
        }
        assert!(!core_b.uploads.pending_target(upload_id, name).exists());
        for seq in (0..chunks).rev() { release_ack(&mut held, seq, false); }
        match upload_event(&mut events).await {
            UploadEvent::Commit(id) => assert_eq!(id, wire_id),
            UploadEvent::Chunk(ack) => panic!("extra boundary chunk {}", ack.seq),
        }
        transfer_progress(&core_a, upload_id, None).await;
        assert_eq!(std::fs::read(core_b.uploads.pending_target(upload_id, name)).unwrap(), bytes);
    }
    {
        let window = window.lock().expect("window");
        assert_eq!(window.max_active, 3);
        assert_eq!(window.active, 0);
        assert_eq!(window.chunks.len(), 15, "no extra chunk at an exact boundary");
        assert_eq!(window.commits.len(), 9);
    }
    core_a.shutdown().await;
    core_b.shutdown().await;
    drop(host);
    relay.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_upload_coalesced_retries_follow_host_move_before_relaying_refs() {
    let (relay_url, relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("attachment host-move directories");
    let runs_c = Arc::new(Mutex::new(Vec::new()));
    let core_b = assemble(&dirs.path().join("b"), "device-b", Arc::default());
    let core_c = assemble(&dirs.path().join("c"), "device-c", runs_c.clone());
    let (host_b, mut events_b, window_b) = gated_upload_host(&core_b, &relay_url);
    let (host_c, mut events_c, window_c) = gated_upload_host(&core_c, &relay_url);
    let core_a = assemble(&dirs.path().join("a"), "device-a", Arc::default());
    attachment_links(&core_a, &relay_url);
    create_attachment_chat(&core_a).await;
    let device_c = core_c.workspace.read_devices().expect("C device rows").into_iter()
        .find(|device| device.id == "device-c").expect("C advertises its actual engine version");
    core_a.workspace.upsert_device_row(&device_c);
    core_c.workspace.rename_chat(CHAT, "Pre-titled").expect("avoid auto-title harness run");
    let upload_id = "hostmove-attachment";
    let wire_id = format!("{upload_id}-q680000");
    let name = "moving host.bin";
    let bytes = attachment_bytes(3 * 510_000);
    stage_attachment(&core_a, upload_id, name, &bytes);
    queue_attachment(&core_a, upload_id, name).await;
    let mut held_b = first_window(&mut events_b).await;
    assert!(core_a.workspace.set_chat_host(CHAT, "device-c").expect("move ownership to C"));
    for _ in 0..3 {
        core_a.doc_host.retry_delivery(CHAT).expect("coalesced retry while B ACKs are held");
    }
    assert!(!core_c.uploads.pending_target(upload_id, name).exists());
    for seq in [2, 0, 1] { release_ack(&mut held_b, seq, false); }
    match upload_event(&mut events_b).await {
        UploadEvent::Commit(id) => assert_eq!(id, wire_id),
        UploadEvent::Chunk(ack) => panic!("duplicate B escort chunk {}", ack.seq),
    }
    // The successful B receipt is stale now. The SAME owner must send C's
    // bytes before relaying the command, rather than trusting B's commit.
    let mut held_c = first_window(&mut events_c).await;
    assert!(held_c.values().all(|ack| ack.upload_id == wire_id && ack.encoded_len == 680_000));
    release_ack(&mut held_c, 1, false);
    release_ack(&mut held_c, 0, false);
    transfer_progress(&core_a, upload_id, Some(2 * 510_000)).await;
    assert!(!core_c.uploads.pending_target(upload_id, name).exists());
    assert!(window_c.lock().expect("C window").commits.is_empty());
    assert_eq!(window_c.lock().expect("C window").relays, 0, "refs cannot outrun C's bytes");
    assert_eq!(window_b.lock().expect("B window").relays, 0, "do not relay to stale B");
    release_ack(&mut held_c, 2, false);
    match upload_event(&mut events_c).await {
        UploadEvent::Commit(id) => assert_eq!(id, wire_id),
        UploadEvent::Chunk(ack) => panic!("duplicate C escort chunk {}", ack.seq),
    }
    transfer_progress(&core_a, upload_id, None).await;
    let logical_ref = zeron_engine::uploads::pending_ref(upload_id, name);
    let resolved = core_c.uploads.resolve_pending(&logical_ref).expect("C resolves unchanged logical ref");
    assert_eq!(std::fs::read(resolved).unwrap(), bytes);
    tokio::time::timeout(Duration::from_secs(30), async {
        while complete_assistant_count(&core_c) != 1 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await.expect("rows-dark command must reach C after the host move");
    assert_eq!(runs_c.lock().expect("C harness requests").len(), 1);
    assert_eq!(complete_assistant_count(&core_b), 0);
    for window in [&window_b, &window_c] {
        let window = window.lock().expect("upload window");
        assert_eq!(window.chunks.len(), 3, "retry must not spawn another upload owner");
        assert_eq!(
            window.commits.as_slice(),
            std::slice::from_ref(&wire_id),
            "same-host bytes are not pushed again"
        );
        assert_eq!(window.max_active, 3);
        assert_eq!(window.active, 0);
    }
    assert_eq!(window_b.lock().expect("B window").relays, 0);
    assert_eq!(window_c.lock().expect("C window").relays, 1);
    assert!(events_b.try_recv().is_err());
    assert!(events_c.try_recv().is_err());
    core_a.shutdown().await;
    core_b.shutdown().await;
    core_c.shutdown().await;
    drop(host_b);
    drop(host_c);
    relay.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_delivery_same_explicit_command_id_runs_independently_on_different_chat_hosts() {
    let (relay_url, relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("cross-chat delivery directories");
    let runs_b = Arc::new(Mutex::new(Vec::new()));
    let runs_c = Arc::new(Mutex::new(Vec::new()));
    let core_b = assemble(&dirs.path().join("b"), "device-b", runs_b.clone());
    let core_c = assemble(&dirs.path().join("c"), "device-c", runs_c.clone());
    let (host_b, mut events_b, window_b) = gated_upload_host(&core_b, &relay_url);
    let (host_c, events_c, window_c) = gated_upload_host(&core_c, &relay_url);
    let core_a = assemble(&dirs.path().join("a"), "device-a", Arc::default());
    attachment_links(&core_a, &relay_url);
    create_attachment_chat(&core_a).await;
    let chat_c = "chat-same-command-on-c";
    let device_c = core_c.workspace.read_devices().expect("C device rows").into_iter()
        .find(|device| device.id == "device-c").expect("C engine device row");
    core_a.workspace.upsert_device_row(&device_c);
    let client = zeron_rpc::memory_client(core_a.rpc_service());
    client.call(methods::MUTATE, serde_json::json!({
        "op": "createChat", "chatId": chat_c, "deviceId": "device-c",
    })).await.expect("create separate chat on C");
    core_b.workspace.rename_chat(CHAT, "Pre-titled B").expect("avoid B auto-title run");
    core_c.workspace.rename_chat(chat_c, "Pre-titled C").expect("avoid C auto-title run");
    let upload_id = "crosschat-attachment";
    let name = "held on B.bin";
    let bytes = attachment_bytes(3 * 510_000);
    stage_attachment(&core_a, upload_id, name, &bytes);
    let request = RunRequest {
        mcp: None,
        prompt: "same explicit command ID, independent chat".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: core_a.uploads.dir().to_string_lossy().into_owned(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: vec![zeron_engine::uploads::pending_ref(upload_id, name)],
        worktree: None,
        resume: None,
    };
    let command_id = "accepted-explicit-command-id";
    let reply_b = client.call(methods::QUEUE_COMMAND, serde_json::json!({
        "chatId": CHAT,
        "commandId": command_id,
        "command": SessionCommandPayload::Run { request: request.clone(), message_id: "same-id-message-b".into() },
        "transfers": [{ "uploadId": upload_id, "fileName": name }],
    })).await.expect("accept explicit ID in B chat");
    assert_eq!(reply_b["commandId"], command_id);
    let mut held_b = first_window(&mut events_b).await;
    let mut request_c = request;
    request_c.attachments.clear();
    let reply_c = client.call(methods::QUEUE_COMMAND, serde_json::json!({
        "chatId": chat_c,
        "commandId": command_id,
        "command": SessionCommandPayload::Run { request: request_c, message_id: "same-id-message-c".into() },
    })).await.expect("same explicit ID is accepted in a different chat");
    assert_eq!(reply_c["commandId"], command_id);
    // B's live owner is deliberately blocked. C must progress independently:
    // command IDs are admitted per chat, not globally across remote hosts.
    tokio::time::timeout(Duration::from_secs(30), async {
        while runs_c.lock().expect("C requests").is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await.expect("C command was suppressed by B's same-ID delivery owner");
    assert!(runs_b.lock().expect("B requests").is_empty());
    assert_eq!(window_b.lock().expect("B window").active, 3);
    assert!(window_b.lock().expect("B window").commits.is_empty());
    assert_eq!(runs_c.lock().expect("C requests").len(), 1);
    assert_eq!(window_c.lock().expect("C window").relays, 1);
    for seq in [2, 0, 1] { release_ack(&mut held_b, seq, false); }
    match upload_event(&mut events_b).await {
        UploadEvent::Commit(id) => assert_eq!(id, format!("{upload_id}-q680000")),
        UploadEvent::Chunk(ack) => panic!("duplicate B escort chunk {}", ack.seq),
    }
    transfer_progress(&core_a, upload_id, None).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        while complete_assistant_count(&core_b) != 1 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await.expect("B command must execute once its own attachment ACKs land");
    assert_eq!(runs_b.lock().expect("B requests").len(), 1);
    assert_eq!(runs_c.lock().expect("C requests").len(), 1);
    assert_eq!(std::fs::read(core_b.uploads.pending_target(upload_id, name)).unwrap(), bytes);
    assert_eq!(window_b.lock().expect("B window").active, 0);
    assert_eq!(window_b.lock().expect("B window").relays, 1);
    assert!(events_b.try_recv().is_err());
    assert!(events_c.is_empty());
    core_a.shutdown().await;
    core_b.shutdown().await;
    core_c.shutdown().await;
    drop(host_b);
    drop(host_c);
    relay.abort();
}
