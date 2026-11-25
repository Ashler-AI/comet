//! Crew: a hosted chat's final tail before a graceful stop (Crew main #243,
//! ported; docs/crew/zgx-thin-port.md).
//!
//! Thin readers see a stopped host's transcript only through its chat2 tail
//! sidecar: Scaffold's agent view of a paused sandbox, and iOS. This host
//! publishes a room's tail on its doc-quiesce tick (`chat2_maintenance`, a
//! second after a change), and Scaffold pauses a sandbox by stopping its
//! headless Crew right after the run, so a turn that ended just before the
//! stop never reached the tail.
//!
//! A deliberate headless stop (a signal or the IPC stop; not sign-out, whose
//! credentials are already revoked) captures the hosted, joined chat2 rooms
//! while their clients exist ([`DocHost::final_tails`]), freezes queues and
//! revokes reachability as before, then publishes each room's tail once,
//! bounded ([`DocHost::publish_final_tails`], lib.rs
//! `shutdown_publishing_tails`). A periodic publish not yet sent when
//! reachability is revoked is dropped, so an older transcript can't land
//! after the final one.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::{ChatDocHandle, DocHost, EdgeConfig, lock, now_ms};

/// Hosted chat2 rooms a graceful stop publishes a final tail for, captured
/// while their room clients still exist ([`DocHost::final_tails`]).
pub struct FinalTails(pub(super) Vec<Arc<ChatDocHandle>>);

impl DocHost {
    /// The rooms a graceful stop must publish a final tail for: chat2 rooms
    /// this engine hosts, has joined and stores on the Edge (a device-hosted,
    /// relaying chat keeps no tail there). Take it before `disconnect_edge`
    /// drops the room clients; none once reachability is already revoked (a
    /// sign-out).
    pub fn final_tails(&self) -> FinalTails {
        if self.inner.config.edge.is_none() || self.inner.edge_disconnected.load(Ordering::Acquire) {
            return FinalTails(Vec::new());
        }
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        FinalTails(
            handles
                .into_iter()
                .filter(|handle| {
                    !handle.retired.load(Ordering::Relaxed)
                        && self.is_host(&handle.chat_id)
                        && lock(&handle.chat2).is_some()
                        && !lock(&handle.relay).as_ref().is_some_and(|state| state.relaying())
                })
                .collect(),
        )
    }

    /// Publish each captured room's tail once, now, bounded by `limit`. Best
    /// effort: a slow or unreachable Edge costs the stop at most `limit`.
    pub async fn publish_final_tails(&self, tails: FinalTails, limit: Duration) {
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let candidates = tails.0.len();
        if candidates == 0 {
            return;
        }
        let started = Instant::now();
        let publishes = tails
            .0
            .into_iter()
            .map(|handle| put_tail(self.inner.http.clone(), edge.clone(), handle));
        match tokio::time::timeout(limit, futures::future::join_all(publishes)).await {
            Ok(published) => tracing::info!(
                candidates,
                published = published.into_iter().filter(|published| *published).count(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "chat2: final tails published before stopping"
            ),
            Err(_) => tracing::warn!(
                candidates,
                limit_ms = limit.as_millis() as u64,
                "chat2: final tail publish timed out; stopping without it"
            ),
        }
    }
}

/// One `PUT /chat2/{chat}/tail`, as `chat2_maintenance` sends it, ahead of
/// background work. Whether the Edge accepted it.
async fn put_tail(http: reqwest::Client, edge: EdgeConfig, handle: Arc<ChatDocHandle>) -> bool {
    // A whale's last 64 joined messages can still contain its entire history:
    // materialized off the network workers.
    let doc = handle.doc.clone();
    let body = tokio::task::spawn_blocking(move || {
        let tail = zeron_doc::materialize_tail(&doc, now_ms(), zeron_doc::TAIL_MESSAGE_COUNT).ok()?;
        serde_json::to_vec(&tail).ok()
    })
    .await
    .ok()
    .flatten();
    let Some(body) = body else {
        return false;
    };
    let Ok(bearer) = edge.bearer().await else {
        return false;
    };
    let url = format!("{}/chat2/{}/tail", edge.url.trim_end_matches('/'), handle.chat_id);
    let Ok(_permit) = zeron_sync::budget::shared()
        .http(zeron_sync::budget::Priority::Interactive)
        .await
    else {
        return false;
    };
    http.put(&url)
        .bearer_auth(&bearer)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}
