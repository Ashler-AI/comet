//! Two independent engines converging through a WebSocket room relay that speaks
//! the same loro-protocol boundary as the SessionRoom Durable Object.

#![allow(clippy::result_large_err)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use loro::{ExportMode, LoroDoc, VersionVector};
use loro_protocol::{
    BatchId, CrdtType, Permission, ProtocolMessage, UpdateStatusCode, decode, encode,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as WsRequest, Response as WsResponse,
};

use comet_doc::{MessagePart, MessageRole, MessageStatus, SessionCommandPayload};
use comet_engine::doc_host::EdgeConfig;
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, RuntimeProfile,
    SandboxLevel, SteeringMode,
};

const PROJECT: &str = "project-shared";
const USER: &str = "user-shared";
const DEPLOYMENT: &str = "deployment-shared";
const TEST_BEARER: &str = "local-test-bearer";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

type ConnectionId = u64;

enum Outbound {
    Binary(Vec<u8>),
    Text(String),
    Close,
}

struct Peer {
    device_id: String,
    joined_loro: bool,
    joined_ephemeral: bool,
    tx: mpsc::UnboundedSender<Outbound>,
}

struct RelayRoom {
    doc: LoroDoc,
    peers: HashMap<ConnectionId, Peer>,
}

impl RelayRoom {
    fn new() -> Self {
        Self {
            doc: LoroDoc::new(),
            peers: HashMap::new(),
        }
    }
}

#[derive(Default)]
struct RelayState {
    rooms: HashMap<String, RelayRoom>,
    blocked_devices: HashSet<String>,
    loro_joins_by_device: HashMap<String, usize>,
}

struct LocalRoomRelay {
    state: Mutex<RelayState>,
    next_connection_id: AtomicU64,
}

impl LocalRoomRelay {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(RelayState::default()),
            next_connection_id: AtomicU64::new(1),
        })
    }

    fn set_blocked(&self, device_id: &str, blocked: bool) {
        let mut state = self.state.lock().expect("relay state");
        if blocked {
            state.blocked_devices.insert(device_id.to_string());
        } else {
            state.blocked_devices.remove(device_id);
        }
    }

    fn is_blocked(&self, device_id: &str) -> bool {
        self.state
            .lock()
            .expect("relay state")
            .blocked_devices
            .contains(device_id)
    }

    fn close_device_connections(&self, device_id: &str) {
        let state = self.state.lock().expect("relay state");
        for room in state.rooms.values() {
            for peer in room
                .peers
                .values()
                .filter(|peer| peer.device_id == device_id)
            {
                let _ = peer.tx.send(Outbound::Close);
            }
        }
    }

    fn connection_count(&self, device_id: &str) -> usize {
        self.state
            .lock()
            .expect("relay state")
            .rooms
            .values()
            .map(|room| {
                room.peers
                    .values()
                    .filter(|peer| peer.device_id == device_id)
                    .count()
            })
            .sum()
    }

    fn loro_join_count(&self, device_id: &str) -> usize {
        self.state
            .lock()
            .expect("relay state")
            .loro_joins_by_device
            .get(device_id)
            .copied()
            .unwrap_or(0)
    }

    fn room_includes(&self, room_id: &str, version: &VersionVector) -> bool {
        self.state
            .lock()
            .expect("relay state")
            .rooms
            .get(room_id)
            .is_some_and(|room| room.doc.oplog_vv().includes_vv(version))
    }

    fn add_peer(
        &self,
        room_id: &str,
        connection_id: ConnectionId,
        device_id: String,
        tx: mpsc::UnboundedSender<Outbound>,
    ) {
        self.state
            .lock()
            .expect("relay state")
            .rooms
            .entry(room_id.to_string())
            .or_insert_with(RelayRoom::new)
            .peers
            .insert(
                connection_id,
                Peer {
                    device_id,
                    joined_loro: false,
                    joined_ephemeral: false,
                    tx,
                },
            );
    }

    fn remove_peer(&self, room_id: &str, connection_id: ConnectionId) {
        if let Some(room) = self
            .state
            .lock()
            .expect("relay state")
            .rooms
            .get_mut(room_id)
        {
            room.peers.remove(&connection_id);
        }
    }

    fn handle_frame(
        &self,
        expected_room_id: &str,
        connection_id: ConnectionId,
        bytes: &[u8],
    ) -> Result<(), String> {
        let message = decode(bytes)?;
        let message_room_id = protocol_room_id(&message);
        if message_room_id != expected_room_id {
            return Err(format!(
                "protocol room {message_room_id:?} did not match routed room {expected_room_id:?}"
            ));
        }

        let mut state = self.state.lock().expect("relay state");
        match message {
            ProtocolMessage::JoinRequest {
                crdt: CrdtType::Loro,
                room_id,
                version,
                ..
            } => {
                let device_id = state
                    .rooms
                    .get(&room_id)
                    .and_then(|room| room.peers.get(&connection_id))
                    .map(|peer| peer.device_id.clone())
                    .ok_or_else(|| "joining peer missing".to_string())?;
                *state.loro_joins_by_device.entry(device_id).or_default() += 1;
                let room = state.rooms.get_mut(&room_id).expect("routed room");
                room.peers
                    .get_mut(&connection_id)
                    .expect("joining peer")
                    .joined_loro = true;
                let server_version = room.doc.oplog_vv();
                send_to(
                    room,
                    connection_id,
                    ProtocolMessage::JoinResponseOk {
                        crdt: CrdtType::Loro,
                        room_id: room_id.clone(),
                        permission: Permission::Write,
                        version: server_version.encode(),
                        extra: None,
                    },
                );
                let backfill = if version.is_empty() {
                    room.doc.export(ExportMode::Snapshot)
                } else {
                    match VersionVector::decode(&version) {
                        Ok(from) => room.doc.export(ExportMode::updates(&from)),
                        Err(_) => room.doc.export(ExportMode::Snapshot),
                    }
                }
                .map_err(|error| format!("backfill export: {error}"))?;
                if !backfill.is_empty() {
                    send_to(
                        room,
                        connection_id,
                        ProtocolMessage::DocUpdate {
                            crdt: CrdtType::Loro,
                            room_id,
                            updates: vec![backfill],
                            batch_id: BatchId([0; 8]),
                        },
                    );
                }
            }
            ProtocolMessage::JoinRequest {
                crdt: CrdtType::LoroEphemeralStore,
                room_id,
                ..
            } => {
                let room = state.rooms.get_mut(&room_id).expect("routed room");
                room.peers
                    .get_mut(&connection_id)
                    .expect("joining peer")
                    .joined_ephemeral = true;
                send_to(
                    room,
                    connection_id,
                    ProtocolMessage::JoinResponseOk {
                        crdt: CrdtType::LoroEphemeralStore,
                        room_id,
                        permission: Permission::Write,
                        version: Vec::new(),
                        extra: None,
                    },
                );
            }
            ProtocolMessage::DocUpdate {
                crdt,
                room_id,
                updates,
                batch_id,
            } => {
                let room = state.rooms.get_mut(&room_id).expect("routed room");
                let joined = room.peers.get(&connection_id).is_some_and(|peer| {
                    (crdt == CrdtType::Loro && peer.joined_loro)
                        || (crdt == CrdtType::LoroEphemeralStore && peer.joined_ephemeral)
                });
                if !joined {
                    send_ack(
                        room,
                        connection_id,
                        crdt,
                        room_id,
                        batch_id,
                        UpdateStatusCode::PermissionDenied,
                    );
                    return Ok(());
                }
                if crdt == CrdtType::Loro
                    && updates
                        .iter()
                        .filter(|update| !update.is_empty())
                        .any(|update| room.doc.import(update).is_err())
                {
                    send_ack(
                        room,
                        connection_id,
                        crdt,
                        room_id,
                        batch_id,
                        UpdateStatusCode::InvalidUpdate,
                    );
                    return Ok(());
                }
                send_ack(
                    room,
                    connection_id,
                    crdt,
                    room_id.clone(),
                    batch_id,
                    UpdateStatusCode::Ok,
                );
                let encoded = encode(&ProtocolMessage::DocUpdate {
                    crdt,
                    room_id,
                    updates,
                    batch_id,
                })
                .map_err(|error| format!("relay encode: {error}"))?;
                for (peer_id, peer) in &room.peers {
                    let peer_joined = (crdt == CrdtType::Loro && peer.joined_loro)
                        || (crdt == CrdtType::LoroEphemeralStore && peer.joined_ephemeral);
                    if *peer_id != connection_id && peer_joined {
                        let _ = peer.tx.send(Outbound::Binary(encoded.clone()));
                    }
                }
            }
            ProtocolMessage::Leave { crdt, room_id } => {
                if let Some(peer) = state
                    .rooms
                    .get_mut(&room_id)
                    .and_then(|room| room.peers.get_mut(&connection_id))
                {
                    if crdt == CrdtType::Loro {
                        peer.joined_loro = false;
                    } else if crdt == CrdtType::LoroEphemeralStore {
                        peer.joined_ephemeral = false;
                    }
                }
            }
            ProtocolMessage::DocUpdateFragmentHeader { .. }
            | ProtocolMessage::DocUpdateFragment { .. } => {
                return Err("focused relay does not accept fragmented test fixtures".into());
            }
            ProtocolMessage::JoinRequest { .. }
            | ProtocolMessage::JoinResponseOk { .. }
            | ProtocolMessage::JoinError { .. }
            | ProtocolMessage::Ack { .. }
            | ProtocolMessage::RoomError { .. } => {}
        }
        Ok(())
    }
}

fn send_to(room: &RelayRoom, connection_id: ConnectionId, message: ProtocolMessage) {
    let bytes = encode(&message).expect("encode relay response");
    if let Some(peer) = room.peers.get(&connection_id) {
        let _ = peer.tx.send(Outbound::Binary(bytes));
    }
}

fn send_ack(
    room: &RelayRoom,
    connection_id: ConnectionId,
    crdt: CrdtType,
    room_id: String,
    ref_id: BatchId,
    status: UpdateStatusCode,
) {
    send_to(
        room,
        connection_id,
        ProtocolMessage::Ack {
            crdt,
            room_id,
            ref_id,
            status,
        },
    );
}

fn protocol_room_id(message: &ProtocolMessage) -> &str {
    match message {
        ProtocolMessage::JoinRequest { room_id, .. }
        | ProtocolMessage::JoinResponseOk { room_id, .. }
        | ProtocolMessage::JoinError { room_id, .. }
        | ProtocolMessage::DocUpdate { room_id, .. }
        | ProtocolMessage::DocUpdateFragmentHeader { room_id, .. }
        | ProtocolMessage::DocUpdateFragment { room_id, .. }
        | ProtocolMessage::Ack { room_id, .. }
        | ProtocolMessage::RoomError { room_id, .. }
        | ProtocolMessage::Leave { room_id, .. } => room_id,
    }
}

fn query_parameter<'a>(uri: &'a str, name: &str) -> Option<&'a str> {
    uri.split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_default()
        .split('&')
        .find_map(|pair| {
            pair.split_once('=')
                .filter(|(key, _)| *key == name)
                .map(|(_, value)| value)
        })
}

fn routed_room_id(uri: &str) -> Option<String> {
    let path = uri.split_once('?').map(|(path, _)| path).unwrap_or(uri);
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["workspace", project, "ws"] if *project == PROJECT => Some(format!("ws4/{project}")),
        ["session", chat, "ws"] if !chat.is_empty() => Some((*chat).to_string()),
        _ => None,
    }
}

fn rejected(status: u16, reason: &str) -> ErrorResponse {
    WsResponse::builder()
        .status(status)
        .body(Some(reason.to_string()))
        .expect("error response")
}

async fn start_relay() -> (String, Arc<LocalRoomRelay>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
    let url = format!("http://{}", listener.local_addr().expect("relay address"));
    let relay = LocalRoomRelay::new();
    let server_relay = relay.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let relay = server_relay.clone();
            tokio::spawn(async move {
                let metadata = Arc::new(Mutex::new(None::<(String, String)>));
                let callback_metadata = metadata.clone();
                let callback_relay = relay.clone();
                let ws = tokio_tungstenite::accept_hdr_async(
                    stream,
                    move |request: &WsRequest, response: WsResponse| {
                        let uri = request.uri().to_string();
                        if query_parameter(&uri, "token") != Some(TEST_BEARER) {
                            return Err(rejected(401, "unauthenticated"));
                        }
                        if query_parameter(&uri, "deploymentId") != Some(DEPLOYMENT) {
                            return Err(rejected(403, "deployment mismatch"));
                        }
                        let Some(device_id) = query_parameter(&uri, "device").map(str::to_string)
                        else {
                            return Err(rejected(400, "device missing"));
                        };
                        if callback_relay.is_blocked(&device_id) {
                            return Err(rejected(503, "device temporarily offline"));
                        }
                        let Some(room_id) = routed_room_id(&uri) else {
                            return Err(rejected(404, "unknown room"));
                        };
                        *callback_metadata.lock().expect("request metadata") =
                            Some((device_id, room_id));
                        Ok(response)
                    },
                )
                .await;
                let Ok(ws) = ws else {
                    return;
                };
                let Some((device_id, room_id)) = metadata.lock().expect("request metadata").take()
                else {
                    return;
                };
                let connection_id = relay.next_connection_id.fetch_add(1, Ordering::Relaxed);
                let (mut sink, mut stream) = ws.split();
                let (tx, mut rx) = mpsc::unbounded_channel();
                relay.add_peer(&room_id, connection_id, device_id, tx.clone());
                let writer = tokio::spawn(async move {
                    while let Some(outbound) = rx.recv().await {
                        let result = match outbound {
                            Outbound::Binary(bytes) => sink.send(WsMessage::Binary(bytes)).await,
                            Outbound::Text(text) => sink.send(WsMessage::Text(text)).await,
                            Outbound::Close => {
                                let _ = sink.send(WsMessage::Close(None)).await;
                                return;
                            }
                        };
                        if result.is_err() {
                            return;
                        }
                    }
                });
                while let Some(Ok(message)) = stream.next().await {
                    match message {
                        WsMessage::Binary(bytes) => {
                            if relay.handle_frame(&room_id, connection_id, &bytes).is_err() {
                                break;
                            }
                        }
                        WsMessage::Text(text) if text == "ping" => {
                            let _ = tx.send(Outbound::Text("pong".into()));
                        }
                        WsMessage::Close(_) => break,
                        _ => {}
                    }
                }
                writer.abort();
                relay.remove_peer(&room_id, connection_id);
            });
        }
    });
    (url, relay, task)
}

struct StreamingHarness;

#[async_trait]
impl Harness for StreamingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "Streaming test harness"
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
        Ok(Vec::new())
    }

    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let events = futures::stream::iter([
            AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "test-model".into(),
                tools: Vec::new(),
                cwd: "/tmp".into(),
                session_id: "native-session".into(),
                assistant_message_id: "assistant-1".into(),
            },
            AgentEvent::TextDelta {
                text: "streamed reply".into(),
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("native-session".into()),
            },
        ])
        .then(|event| async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(event)
        })
        .boxed();
        Ok(events)
    }
}

fn registry() -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::for_profile(RuntimeProfile::Mock);
    registry.register(Arc::new(StreamingHarness));
    Arc::new(registry)
}

fn assemble_remote(dir: &std::path::Path, device_id: &str, edge_url: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create engine directory");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    let edge = EdgeConfig::with_static_token(edge_url, TEST_BEARER)
        .with_device(device_id)
        .with_deployment(DEPLOYMENT);
    EngineCore::assemble_with_identity(
        dir,
        registry(),
        HarnessId::Mock,
        Some(edge),
        PROJECT,
        USER,
        RuntimeProfile::Mock,
    )
    .expect("assemble remote engine")
}

fn run_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

async fn wait_for(mut condition: impl FnMut() -> bool, what: &str) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn entry_contains(entry: &comet_doc::SessionMessageEntry, text: &str) -> bool {
    entry
        .parts
        .iter()
        .any(|part| matches!(part, MessagePart::Text { text: value, .. } if value == text))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_authenticated_engines_sync_workspace_streams_and_reconnect_backfill() {
    let (edge_url, relay, relay_task) = start_relay().await;
    let dirs = tempfile::tempdir().expect("tempdir");
    let a = assemble_remote(&dirs.path().join("a"), "device-a", &edge_url);
    let b = assemble_remote(&dirs.path().join("b"), "device-b", &edge_url);

    wait_for(
        || a.workspace.connected() && b.workspace.connected(),
        "both workspace room joins",
    )
    .await;
    wait_for(
        || {
            [(&a, "device-b"), (&b, "device-a")]
                .into_iter()
                .all(|(engine, peer)| {
                    engine
                        .workspace
                        .doc()
                        .read_devices()
                        .unwrap_or_default()
                        .iter()
                        .any(|device| device.id == peer)
                })
        },
        "workspace device rows in both directions",
    )
    .await;

    a.workspace
        .create_space("space-a", "device-a", "/tmp", None, false)
        .expect("create shared space");
    a.workspace
        .create_chat("chat-shared", "space-a", None, None)
        .expect("create shared chat");
    a.workspace
        .set_chat_harness_session("chat-shared", "native-session", "/tmp");
    wait_for(
        || {
            b.workspace
                .doc()
                .chat("chat-shared")
                .ok()
                .flatten()
                .is_some_and(|chat| {
                    chat.device_id == "device-a"
                        && chat.harness_session_id.as_deref() == Some("native-session")
                })
        },
        "A's chat/session row on B",
    )
    .await;

    let handle_a = a.doc_host.open("chat-shared").expect("open A chat");
    let handle_b = b.doc_host.open("chat-shared").expect("open B chat");
    let mut messages_b = handle_b.watch_messages();
    wait_for(
        || handle_a.connected() && handle_b.connected(),
        "both session room joins",
    )
    .await;

    a.doc_host
        .queue_command(
            "chat-shared",
            SessionCommandPayload::Run {
                request: run_request("prompt from A"),
                message_id: "user-1".into(),
            },
        )
        .expect("queue A prompt");

    let mut saw_remote_streaming = false;
    let stream_result = tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            {
                let messages = messages_b.borrow_and_update();
                saw_remote_streaming |= messages.iter().any(|entry| {
                    entry.role == MessageRole::Assistant
                        && entry.status == Some(MessageStatus::Streaming)
                });
                let complete = messages.iter().any(|entry| {
                    entry.role == MessageRole::Assistant
                        && entry.status == Some(MessageStatus::Complete)
                        && entry_contains(entry, "streamed reply")
                });
                if saw_remote_streaming && complete {
                    break;
                }
            }
            messages_b
                .changed()
                .await
                .expect("message watch stays open");
        }
    })
    .await;
    assert!(
        stream_result.is_ok(),
        "B did not receive streaming + complete state; saw_streaming={saw_remote_streaming}, A={:?}, B={:?}, commands={:?}",
        handle_a.doc().read_entries().unwrap_or_default(),
        handle_b.doc().read_entries().unwrap_or_default(),
        handle_a.doc().read_commands().unwrap_or_default()
    );

    b.workspace
        .rename_chat("chat-shared", "renamed by device B")
        .expect("B renames shared chat");
    wait_for(
        || {
            a.workspace
                .doc()
                .chat("chat-shared")
                .ok()
                .flatten()
                .and_then(|chat| chat.title)
                .as_deref()
                == Some("renamed by device B")
        },
        "B's workspace state change on A",
    )
    .await;

    let joins_before = relay.loro_join_count("device-b");
    relay.set_blocked("device-b", true);
    relay.close_device_connections("device-b");
    wait_for(
        || relay.connection_count("device-b") == 0,
        "B's room sockets to close",
    )
    .await;

    handle_a
        .write_user_message("user-during-reconnect", "missed live broadcast", 2_000)
        .expect("write while B disconnected");
    a.workspace
        .set_chat_archived("chat-shared", true)
        .expect("archive while B disconnected");
    let session_version = handle_a.doc().doc().oplog_vv();
    let workspace_version = a.workspace.doc().doc().oplog_vv();
    wait_for(
        || {
            relay.room_includes("chat-shared", &session_version)
                && relay.room_includes(&format!("ws4/{PROJECT}"), &workspace_version)
        },
        "relay to persist A's disconnected-window updates",
    )
    .await;
    assert!(
        !handle_b
            .doc()
            .read_entries()
            .unwrap_or_default()
            .iter()
            .any(|entry| entry.id == "user-during-reconnect"),
        "blocked B must miss the live session broadcast"
    );

    relay.set_blocked("device-b", false);
    wait_for(
        || relay.loro_join_count("device-b") >= joins_before + 2,
        "B to rejoin both workspace and session rooms",
    )
    .await;
    wait_for(
        || {
            handle_b
                .doc()
                .read_entries()
                .unwrap_or_default()
                .iter()
                .any(|entry| {
                    entry.id == "user-during-reconnect"
                        && entry_contains(entry, "missed live broadcast")
                })
        },
        "session VV backfill on B",
    )
    .await;
    wait_for(
        || {
            b.workspace
                .doc()
                .chat("chat-shared")
                .ok()
                .flatten()
                .is_some_and(|chat| chat.archived)
        },
        "workspace VV backfill on B",
    )
    .await;

    a.shutdown().await;
    b.shutdown().await;
    relay_task.abort();
}
