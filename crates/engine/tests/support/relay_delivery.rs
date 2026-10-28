// Shared by the engine relay tests and the UI's real-engine composer regression.
// tungstenite fixes the handshake callback's error type as a full Response.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as WsRequest, Response as WsResponse,
};

use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, Device, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
};
use zeron_rpc::{
    DeviceFrameHeader, LinkCache, LinkCacheConfig, StaticToken, decode_device_frame,
    encode_device_frame, methods,
};

pub(crate) const CHAT: &str = "chat-relay-fallback";

// Minimal in-memory device room (route-only subset of the DO semantics) —
// same shape as device_routing.rs.
#[derive(Default)]
struct RelayState {
    host: Option<mpsc::UnboundedSender<Vec<u8>>>,
    clients: HashMap<String, mpsc::UnboundedSender<Vec<u8>>>,
}

pub(crate) async fn fake_device_room() -> (String, tokio::task::JoinHandle<()>) {
    observed_device_room(None).await
}

pub(crate) async fn observed_device_room(
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

pub(crate) fn assemble(
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

pub(crate) fn attachment_links(core: &EngineCore, relay_url: &str) {
    let mut config = LinkCacheConfig::new(
        relay_url.to_owned(), Arc::new(StaticToken("test-user".into())),
    );
    config.probe_timeout = Duration::from_secs(5);
    core.set_links(LinkCache::new(config));
}

pub(crate) async fn create_attachment_chat(core: &EngineCore) {
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
