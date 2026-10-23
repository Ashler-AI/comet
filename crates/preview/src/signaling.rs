//! The coordinator receives presence, catalogs and SDP only. Reconnects obtain
//! fresh credentials and clear the previous connection's remote routes/peers.
use crate::{
    catalog::Catalog,
    peer::{OutgoingSignal, Peers, Signal},
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tokio_util::sync::CancellationToken;
use zeron_proto::{AccessToken, PreviewService};
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> anyhow::Result<AccessToken>;
    fn invalidate(&self, generation: u64);
}
pub struct Config {
    pub edge_url: String,
    pub org_id: String,
    pub tokens: Arc<dyn TokenSource>,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Incoming {
    Catalog {
        device: String,
        services: Vec<PreviewService>,
    },
    Gone {
        device: String,
    },
    Signal {
        from: String,
        signal: Signal,
    },
}
pub async fn run(
    config: Config,
    catalog: Catalog,
    peers: Peers,
    mut outgoing: mpsc::Receiver<OutgoingSignal>,
    stop: CancellationToken,
) {
    loop {
        let connected = connect(&config, &catalog, &peers, &mut outgoing);
        tokio::select! { _ = stop.cancelled() => break, result = connected => {
            if let Err(error) = result { tracing::debug!(%error, "preview coordinator disconnected"); }
        } }
        catalog.clear_remote();
        peers.clear().await;
        while outgoing.try_recv().is_ok() {} // signaling from the old lease is stale
        tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(3)) => {} }
    }
    catalog.clear_remote();
    peers.clear().await;
}
async fn connect(
    config: &Config,
    catalog: &Catalog,
    peers: &Peers,
    outgoing: &mut mpsc::Receiver<OutgoingSignal>,
) -> anyhow::Result<()> {
    let token = config.tokens.token().await?;
    let mut url = reqwest::Url::parse(&config.edge_url)?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid preview coordinator URL"))?;
    url.set_path(&format!("/preview/{}/ws", config.org_id));
    url.query_pairs_mut()
        .clear()
        .append_pair("device", catalog.device_id());
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {}", token.value).parse()?);
    let mut websocket_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    websocket_config.max_message_size = Some(1024 * 1024);
    websocket_config.max_frame_size = Some(1024 * 1024);
    let handshake = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async_with_config(request, Some(websocket_config), false),
    )
    .await?;
    if let Err(tokio_tungstenite::tungstenite::Error::Http(response)) = &handshake {
        if response.status() == 401 {
            config.tokens.invalidate(token.generation);
        }
    }
    let (socket, _) = handshake?;
    let (mut write, mut read) = socket.split();
    let mut changes = catalog.subscribe();
    let mut advertised = catalog.local_services();
    write
        .send(Message::Text(
            serde_json::json!({"type":"catalog", "services":advertised}).to_string(),
        ))
        .await?;
    let mut ping = tokio::time::interval(Duration::from_secs(10));
    let mut last_message = tokio::time::Instant::now();
    // SDP gathering must not block the socket heartbeat or other devices.
    let mut negotiations = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = ping.tick() => {
                anyhow::ensure!(last_message.elapsed() < Duration::from_secs(40), "preview coordinator lease expired");
                write.send(Message::Text("ping".into())).await?;
            }
            _ = changes.changed() => {
                let next = catalog.local_services();
                if next != advertised { write.send(Message::Text(serde_json::json!({"type":"catalog", "services":next}).to_string())).await?; advertised = next; }
            }
            Some(message) = outgoing.recv() => {
                write.send(Message::Text(serde_json::json!({"type":"signal", "to":message.to, "signal":message.signal}).to_string())).await?;
            }
            Some(_) = negotiations.join_next(), if !negotiations.is_empty() => {}
            message = read.next() => {
                let message = message.ok_or_else(|| anyhow::anyhow!("preview coordinator closed"))??;
                last_message = tokio::time::Instant::now();
                match message {
                    Message::Text(text) if text == "pong" => {},
                    Message::Text(text) => {
                        anyhow::ensure!(text.len() <= 1024*1024, "oversized preview coordinator message");
                        match serde_json::from_str::<Incoming>(&text)? {
                            Incoming::Catalog { device, services } => catalog.set_remote(&device, services)?,
                            Incoming::Gone { device } => { catalog.remove_remote(&device); peers.remove(&device).await; }
                            Incoming::Signal { from, signal } => {
                                anyhow::ensure!(negotiations.len() < 16, "too many concurrent preview negotiations");
                                let peers = peers.clone();
                                negotiations.spawn(async move { if let Err(error) = peers.signal(&from, signal).await { tracing::debug!(%error, "preview negotiation failed"); } });
                            }
                        }
                    }
                    Message::Ping(bytes) => write.send(Message::Pong(bytes)).await?,
                    Message::Close(frame) => {
                        if frame.is_some_and(|frame| u16::from(frame.code) == 4401) {
                            config.tokens.invalidate(token.generation);
                        }
                        anyhow::bail!("preview coordinator closed");
                    }
                    Message::Binary(_) => anyhow::bail!("application traffic is not allowed on preview signaling"),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::{BoxIo, Connector};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::{io::AsyncReadExt, net::TcpListener};
    use tokio_tungstenite::tungstenite::{
        handshake::server::{Request, Response},
        protocol::{CloseFrame, frame::coding::CloseCode},
    };

    struct Tokens {
        generation: AtomicU64,
        invalidated: AtomicU64,
    }

    #[async_trait::async_trait]
    impl TokenSource for Tokens {
        async fn token(&self) -> anyhow::Result<AccessToken> {
            let generation = self.generation.load(Ordering::SeqCst);
            Ok(AccessToken { value: format!("lease-{generation}"), generation })
        }

        fn invalidate(&self, generation: u64) {
            assert_eq!(self.invalidated.swap(generation, Ordering::SeqCst), 0);
            let _ = self.generation.compare_exchange(
                generation, generation + 1, Ordering::SeqCst, Ordering::SeqCst,
            );
        }
    }

    struct NoServices;

    #[async_trait::async_trait]
    impl Connector for NoServices {
        async fn connect(&self, _: &str) -> anyhow::Result<BoxIo> {
            panic!("signaling must not open application traffic");
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Outcome {
        Http(u16),
        Close(u16),
        Timeout,
    }

    #[tokio::test]
    async fn only_explicit_auth_rejections_invalidate_the_dialed_generation() {
        for outcome in [
            Outcome::Http(401),
            Outcome::Http(403),
            Outcome::Http(429),
            Outcome::Http(503),
            Outcome::Close(4401),
            Outcome::Close(4403),
            Outcome::Close(1013),
            Outcome::Close(1000),
            Outcome::Timeout,
        ] {
            let tokens = Arc::new(Tokens {
                generation: AtomicU64::new(7),
                invalidated: AtomicU64::new(0),
            });
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server_tokens = tokens.clone();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                if matches!(outcome, Outcome::Timeout) {
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") {
                        headers.push(socket.read_u8().await.unwrap());
                    }
                    assert!(String::from_utf8(headers).unwrap().to_lowercase()
                        .contains("authorization: bearer lease-7\r\n"));
                    server_tokens.generation.store(8, Ordering::SeqCst);
                    // Keep the handshake unanswered until the client times out.
                    socket.read_to_end(&mut Vec::new()).await.unwrap();
                    return;
                }
                let handshake = tokio_tungstenite::accept_hdr_async(
                    socket,
                    move |request: &Request, response: Response| {
                        assert_eq!(request.headers()["authorization"], "Bearer lease-7");
                        // Simulate another transport refreshing before this response.
                        server_tokens.generation.store(8, Ordering::SeqCst);
                        if let Outcome::Http(status) = outcome {
                            let mut rejection = response.map(|_| Some(String::new()));
                            *rejection.status_mut() = status.try_into().unwrap();
                            return Err(rejection);
                        }
                        Ok(response)
                    },
                ).await;
                if let Outcome::Close(code) = outcome {
                    let mut socket = handshake.unwrap();
                    assert!(socket.next().await.unwrap().unwrap().is_text());
                    socket.send(Message::Close(Some(CloseFrame {
                        code: CloseCode::from(code),
                        reason: "test outcome".into(),
                    }))).await.unwrap();
                    // Leave the TCP connection open until the client reads the close.
                    while let Some(Ok(message)) = socket.next().await {
                        if message.is_close() { break; }
                    }
                } else {
                    assert!(handshake.is_err());
                }
            });
            let temp = tempfile::tempdir().unwrap();
            let catalog = Catalog::open(
                temp.path().join("catalog.json"), "device".into(), "Device".into(),
            ).unwrap();
            let stop = CancellationToken::new();
            let (peers, mut outgoing) = Peers::new(
                "device".into(), Arc::new(NoServices), stop.clone(),
            );
            let config = Config {
                edge_url: format!("http://{address}"),
                org_id: "org".into(),
                tokens: tokens.clone(),
            };
            let error = tokio::time::timeout(
                Duration::from_secs(15),
                connect(&config, &catalog, &peers, &mut outgoing),
            ).await.expect("signaling did not finish").unwrap_err();
            if matches!(outcome, Outcome::Timeout) {
                assert!(error.downcast_ref::<tokio::time::error::Elapsed>().is_some());
            }
            server.await.unwrap();
            let expected = if matches!(outcome, Outcome::Http(401) | Outcome::Close(4401)) {
                7
            } else {
                0
            };
            assert_eq!(tokens.invalidated.load(Ordering::SeqCst), expected, "{outcome:?}");
            assert_eq!(tokens.generation.load(Ordering::SeqCst), 8, "{outcome:?}");
            stop.cancel();
        }
    }
}
