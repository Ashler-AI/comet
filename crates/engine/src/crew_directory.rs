//! Read-only transcript projection into the owner-scoped Scaffold directory.
//! Registry sequence numbers fence acknowledged fields; Loro VV fences links.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use loro::{Container, LoroValue, ValueOrContainer};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeron_sync::DocsStore;

use crate::{DocHost, EngineError, WorkspaceHost, auth::{Auth, AuthState}, crew::CrewServices, scaffold::ScaffoldError};

type Version = BTreeMap<String, u64>;
const JOB: &str = "crew-directory";
const REGISTRY_VERSION: &str = "r:registry1";
const RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
enum PublishError {
    #[error(transparent)]
    Row(#[from] EngineError),
    #[error("directory {operation}: {source}")]
    Remote { operation: &'static str, source: ScaffoldError },
}

// Elapsed time avoids overflowing Instant for a very large Retry-After. Sleep in
// bounded chunks without doing any work or allowing notifications to cut it short.
async fn wait_for_retry(started: tokio::time::Instant, delay: Duration) {
    loop {
        let remaining = delay.saturating_sub(started.elapsed());
        if remaining.is_zero() { return; }
        tokio::time::sleep(remaining.min(Duration::from_secs(24 * 60 * 60))).await;
    }
}

async fn wait_for_work(
    registry: &mut tokio::sync::watch::Receiver<u64>,
    snapshots: &mut tokio::sync::watch::Receiver<u64>,
    identity: &mut tokio::sync::watch::Receiver<AuthState>,
    cooldown: Option<Duration>,
) -> Option<bool> {
    let started = tokio::time::Instant::now();
    let mut inventory = false;
    loop {
        tokio::select! {
            biased;
            _ = wait_for_retry(started, cooldown.unwrap_or_default()), if cooldown.is_some() => return Some(inventory),
            result = registry.changed() => { if result.is_err() { return None; } inventory = true; },
            result = snapshots.changed() => { if result.is_err() { return None; } },
            result = identity.changed() => { if result.is_err() { return None; } inventory = true; },
        }
        if cooldown.is_none() { return Some(inventory); }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Evidence {
    source_version: Version,
    confirmed: bool,
    // A local candidate already rejected by the monotonic remote version cannot
    // become publishable until the local source/registry version changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rejected_source_version: Option<Version>,
}

fn failure(message: &str) -> EngineError { EngineError::Other(message.into()) }

/// One bounded worker. The engine owns/aborts this handle before profile shutdown.
/// No chat is opened, attached, claimed, or executed to make it discoverable.
pub(crate) fn start(auth: Auth, services: CrewServices, workspace: WorkspaceHost,
    doc_host: DocHost) -> tokio::task::JoinHandle<()> {
    let store = doc_host.crew_store();
    let mut registry = workspace.watch_registry();
    let mut snapshots = store.watch_directory();
    let mut identity = auth.watch_state();
    tokio::spawn(async move {
        let mut inventory = true;
        // Remote versions are prerequisites only, never local projection evidence.
        let mut required_sources = BTreeMap::new();
        loop {
            // Changes accumulated during backpressure still require inventory.
            inventory |= registry.has_changed().unwrap_or(false) || identity.has_changed().unwrap_or(false);
            registry.borrow_and_update();
            snapshots.borrow_and_update();
            identity.borrow_and_update();
            let mut cooldown = RETRY_DELAY;
            if services.ensure_identity().is_ok() && workspace.registry_synced() {
                if inventory {
                    if let Err(error) = enqueue_registry(&store, &workspace).await {
                        tracing::warn!(%error, "directory registry inventory retained for retry");
                    } else {
                        inventory = false;
                    }
                }
                match drain(&store, &services, &workspace, &mut required_sources).await {
                    Ok(Some(delay)) => cooldown = delay,
                    Ok(None) => {},
                    Err(error) => tracing::warn!(%error, "directory pending work retained for retry"),
                }
            }
            // The worker owns the deadline, not the notification that wakes it.
            // Inventory's own queue writes must not cause a warm retry either.
            let pending = store.pending_sync_jobs(JOB, "", 1).map_or(true, |ids| !ids.is_empty());
            let Some(changed) = wait_for_work(&mut registry, &mut snapshots, &mut identity,
                (pending || inventory).then_some(cooldown)).await else { return };
            inventory |= changed;
        }
    })
}

async fn enqueue_registry(store: &DocsStore, workspace: &WorkspaceHost) -> Result<(), EngineError> {
    let mut after = String::new();
    loop {
        let rows = workspace.acknowledged_chats_after(&after);
        if rows.is_empty() { return Ok(()); }
        for row in rows {
            after = row.id;
            if uuid::Uuid::parse_str(&after).is_ok() { store.queue_directory(&after)?; }
        }
        tokio::task::yield_now().await;
    }
}

async fn drain(store: &Arc<DocsStore>, services: &CrewServices, workspace: &WorkspaceHost,
    required_sources: &mut BTreeMap<String, Version>) -> Result<Option<Duration>, EngineError> {
    drain_pending(store, || services.ensure_identity(), required_sources, |id, mut required| async move {
        let result = publish(store, services, workspace, &id, &mut required).await;
        if matches!(result, Ok(true)) { required.remove(&id); }
        (required, result)
    }).await
}

async fn drain_pending<S: Default, Fut>(
    store: &DocsStore,
    ensure_identity: impl Fn() -> Result<(), EngineError>,
    state: &mut S,
    mut publish: impl FnMut(String, S) -> Fut,
) -> Result<Option<Duration>, EngineError>
where Fut: Future<Output = (S, Result<bool, PublishError>)> {
    let mut after = String::new();
    loop {
        let ids = store.pending_sync_jobs(JOB, &after, 64)?;
        if ids.is_empty() { return Ok(None); }
        for id in ids {
            after.clone_from(&id);
            ensure_identity()?;
            let Some(generation) = store.sync_job_version(&id, JOB)? else { continue };
            let (next_state, result) = publish(id, std::mem::take(state)).await;
            *state = next_state;
            match result {
                Ok(true) => store.complete_sync_job(&after, JOB, generation)?,
                Ok(false) => {}, // Keep the durable job until its own source catches up.
                Err(error) => {
                    tracing::warn!(session = %after, %error, "directory update awaits synchronization or reconciliation");
                    if let PublishError::Remote { source, .. } = error
                        && source.is_transient() {
                        return Ok(Some(source.retry_after().unwrap_or(RETRY_DELAY).max(RETRY_DELAY)));
                    }
                },
            }
        }
        tokio::task::yield_now().await;
    }
}

async fn lookup_remote(
    store: &DocsStore, client: &crate::scaffold::ScaffoldClient, id: &str,
    deployment: Option<&str>, version: &Version, evidence: &mut Evidence,
    required_sources: &mut BTreeMap<String, Version>,
) -> Result<Value, PublishError> {
    if evidence.rejected_source_version.as_ref() == Some(version) {
        return Err(failure("directory source unchanged since remote version rejection; awaiting synchronization").into());
    }
    let remote = client.get_crew_session(id, deployment).await
        .map_err(|source| PublishError::Remote { operation: "revision lookup", source })?;
    let previous = remote.get("session").ok_or_else(|| failure("invalid directory response"))?;
    if !previous.is_null() {
        let observed: Version = serde_json::from_value(previous["sourceVersion"].clone())
            .map_err(|_| failure("invalid directory server version"))?;
        // Invalid remote data is not evidence that a local candidate is stale.
        require_observed(&observed, &Version::new())?;
        if let Err(error) = require_observed(version, &observed) {
            required_sources.insert(id.to_owned(), observed);
            evidence.rejected_source_version = Some(version.clone());
            store.save_directory_state(id, &serde_json::to_string(evidence)
                .map_err(|_| failure("invalid directory evidence"))?).map_err(EngineError::from)?;
            return Err(error.into());
        }
    }
    evidence.rejected_source_version = None;
    Ok(remote)
}

async fn publish(store: &Arc<DocsStore>, services: &CrewServices, workspace: &WorkspaceHost,
    id: &str, required_sources: &mut BTreeMap<String, Version>) -> Result<bool, PublishError> {
    if workspace.directory_reconciliation_required() {
        return Err(failure("registry server reset observed; directory reconciliation required").into());
    }
    let Some(row) = workspace.acknowledged_chat(id) else {
        // Absence is not a deletion; optimistic creations await their server ACK.
        return Err(failure("directory awaits acknowledged registry row").into());
    };
    if row.seq == 0 { return Err(failure("directory awaits server registry sequence").into()); }
    let expected = services.session_metadata(id)?;
    let deployment = expected.scope.deployment_id.as_deref()
        .ok_or_else(|| failure("directory requires exact deployment"))?;
    if deployment != services.runtime().deployment_id() {
        return Err(failure("directory runtime differs from verified deployment").into());
    }
    if !row.deleted {
        // Phones create crew:None. Stamp only through the shared, identity-checked
        // adoption path, then wait for the server to acknowledge that metadata.
        services.adopt_chat(workspace, id)?;
        let metadata: zeron_proto::CrewChatMetadata = serde_json::from_value(
            row.fields.get("crew").cloned().unwrap_or(Value::Null))
            .map_err(|_| failure("directory awaits acknowledged Crew metadata"))?;
        metadata.validate().map_err(failure)?;
        if metadata.owner_subject != expected.owner_subject || metadata.scope != expected.scope
            || metadata.session_id != id {
            return Err(failure("directory owner or scope mismatch").into());
        }
    }
    let mut evidence: Evidence = match store.directory_state(id).map_err(EngineError::from)? {
        Some(saved) => serde_json::from_str(&saved).map_err(|_| failure("invalid directory evidence"))?,
        None => Evidence::default(),
    };
    // A deletion can use only previously observed local transcript evidence,
    // never counters copied out of a directory response.
    let projection_store = store.clone();
    let projection_id = id.to_owned();
    let projection = tokio::task::spawn_blocking(move || {
        projection_store.load_directory_snapshot(&projection_id)?
            .map(|bytes| project(&bytes)).transpose()
    }).await.map_err(|_| failure("directory snapshot projection failed"))??;
    let (mut version, mut links) = projection.unwrap_or_else(|| {
        (if row.deleted { evidence.source_version.clone() } else { Version::new() }, Vec::new())
    });
    if row.deleted { links.clear(); }
    version.insert(REGISTRY_VERSION.into(), row.seq);
    require_observed(&version, &Version::new())?;
    if require_observed(&version, &evidence.source_version).is_err()
        || required_sources.get(id).is_some_and(|required| require_observed(&version, required).is_err()) {
        return Ok(false);
    }
    if evidence.confirmed && evidence.source_version == version { return Ok(true); }
    services.ensure_identity()?;
    let client = services.runtime().client();
    let remote = lookup_remote(store, &client, id, Some(deployment), &version, &mut evidence, required_sources).await?;
    let previous = remote.get("session").ok_or_else(|| failure("invalid directory response"))?;
    let revision = if !previous.is_null() {
        if previous.get("deleted").and_then(Value::as_bool) == Some(true) { return Ok(true); }
        Some(previous["revision"].as_u64().filter(|revision| *revision > 0)
            .ok_or_else(|| failure("invalid directory server revision"))?)
    } else { None };
    // Do not send a projection captured before a registry ACK arriving during
    // HTTP lookup. Concurrent later writers still meet expectedRevision at SQL.
    if workspace.directory_reconciliation_required() {
        return Err(failure("registry server reset observed; directory reconciliation required").into());
    }
    if workspace.acknowledged_chat(id).as_ref() != Some(&row) {
        return Err(failure("directory registry changed during projection").into());
    }
    evidence.source_version = version;
    evidence.confirmed = false;
    store.save_directory_state(id, &serde_json::to_string(&evidence)
        .map_err(|_| failure("invalid directory evidence"))?).map_err(EngineError::from)?;
    services.ensure_identity()?;
    // Platform authorizes upsert via the read-only existing-chat2 owner endpoint.
    // Empty new chats with no room owner remain pending; this never creates one.
    let ack = client.upsert_crew_session(&json!({
        "sessionId": id, "deploymentId": deployment, "expectedRevision": revision,
        "sourceVersion": evidence.source_version, "generatedTitle": null,
        "title": row.fields.get("title").and_then(Value::as_str).unwrap_or_default()
            .chars().take(140).collect::<String>(),
        "links": links, "deleted": row.deleted,
    })).await.map_err(|source| PublishError::Remote { operation: "upsert", source })?;
    if ack["revision"].as_u64().filter(|revision| *revision > 0).is_none() {
        return Err(failure("invalid directory acknowledgement").into());
    }
    evidence.confirmed = true;
    store.save_directory_state(id, &serde_json::to_string(&evidence)
        .map_err(|_| failure("invalid directory evidence"))?).map_err(EngineError::from)?;
    Ok(true)
}

fn require_observed(candidate: &Version, previous: &Version) -> Result<(), EngineError> {
    if candidate.len() > 128 || candidate.values().any(|counter| *counter > 9_007_199_254_740_991) {
        return Err(failure("directory source version exceeds wire bounds"));
    }
    if candidate.get(REGISTRY_VERSION).copied().unwrap_or(0)
        < previous.get(REGISTRY_VERSION).copied().unwrap_or(0) {
        return Err(failure("registry sequence regressed; directory reconciliation required"));
    }
    if previous.iter().any(|(peer, counter)| candidate.get(peer).copied().unwrap_or(0) < *counter) {
        return Err(failure("directory source is older or concurrent; awaiting synchronized snapshot"));
    }
    Ok(())
}

fn field_string(map: &loro::LoroMap, key: &str) -> Option<String> {
    match map.get(key) {
        Some(ValueOrContainer::Value(LoroValue::String(value))) => Some(value.to_string()),
        _ => None,
    }
}

/// Original Crew source-link extraction, without title inference or full-history
/// materialization. Canonical Slack/Notion keys are shared by server upsert/search.
fn project(bytes: &[u8]) -> Result<(Version, Vec<String>), EngineError> {
    let doc = loro::LoroDoc::new();
    doc.import(bytes).map_err(|_| failure("invalid directory snapshot"))?;
    let version = doc.oplog_vv().iter()
        .map(|(peer, counter)| (format!("s:{peer}"), *counter as u64)).collect();
    let mut links = BTreeSet::new();
    let messages = doc.get_list("messages");
    for index in 0..messages.len() {
        let Some(ValueOrContainer::Container(Container::Map(row))) = messages.get(index) else {
            return Err(failure("malformed directory transcript row"));
        };
        if field_string(&row, "role").as_deref() == Some("system") { continue; }
        let Some(ValueOrContainer::Container(Container::List(parts))) = row.get("parts") else { continue };
        for index in 0..parts.len() {
            let Some(ValueOrContainer::Container(Container::Map(part))) = parts.get(index) else { continue };
            match field_string(&part, "kind").as_deref() {
                Some("text") => {
                    let mut carry = String::new();
                    match part.get("text") {
                        Some(ValueOrContainer::Container(Container::Text(text))) => {
                            let mut start = 0;
                            while start < text.len_utf8() {
                                let mut end = (start + 64 * 1024).min(text.len_utf8());
                                let delta = loop {
                                    match text.slice_delta(start, end, loro::cursor::PosType::Bytes) {
                                        Ok(delta) => break delta,
                                        Err(_) if end > start + 1 => end -= 1,
                                        Err(_) => return Err(failure("invalid directory text boundary")),
                                    }
                                };
                                for delta in delta {
                                    if let loro::TextDelta::Insert { insert, .. } = delta {
                                        scan_chunk(&mut carry, &insert, false, &mut links)?;
                                    }
                                }
                                start = end;
                            }
                        }
                        Some(ValueOrContainer::Value(LoroValue::String(text))) => {
                            extract_links(&text, &mut links)?;
                        }
                        _ => {}
                    }
                    scan_chunk(&mut carry, "", true, &mut links)?;
                }
                Some("tool") => {
                    if let Some(ValueOrContainer::Value(value)) = part.get("call") {
                        tool_links(&value, &mut links, 0)?;
                    }
                }
                _ => {},
            }
        }
    }
    Ok((version, links.into_iter().collect()))
}

fn tool_links(value: &LoroValue, links: &mut BTreeSet<String>, depth: usize) -> Result<(), EngineError> {
    if depth > 64 { return Err(failure("directory tool content exceeds nesting bound")); }
    match value {
        LoroValue::String(value) => extract_links(value, links)?,
        LoroValue::List(items) => for value in items.iter() { tool_links(value, links, depth + 1)?; },
        LoroValue::Map(fields) => for (key, value) in fields.iter() {
            if !matches!(key.as_str(), "reasoning" | "thinking" | "system" | "systemPrompt") {
                tool_links(value, links, depth + 1)?;
            }
        },
        _ => {},
    }
    Ok(())
}

fn url_separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '|' | ')' | ']')
}

fn scan_chunk(carry: &mut String, chunk: &str, finished: bool, links: &mut BTreeSet<String>)
    -> Result<(), EngineError> {
    carry.push_str(chunk);
    let split = if finished { carry.len() } else {
        carry.char_indices().rev().find(|(_, ch)| url_separator(*ch))
            .map_or(0, |(index, ch)| index + ch.len_utf8())
    };
    extract_links(&carry[..split], links)?;
    carry.drain(..split);
    if carry.len() > 4096 {
        extract_links(carry, links)?;
        let mut suffix = carry.len().saturating_sub(8);
        while !carry.is_char_boundary(suffix) { suffix += 1; }
        carry.drain(..suffix);
    }
    Ok(())
}

fn extract_links(text: &str, links: &mut BTreeSet<String>) -> Result<(), EngineError> {
    for (start, _) in text.match_indices("https://").chain(text.match_indices("http://")) {
        let candidate = text[start..].split(url_separator).next().unwrap_or_default()
            .trim_end_matches(['.', ',', ';', ':']);
        let Ok(url) = reqwest::Url::parse(candidate) else { continue };
        let host = url.host_str().unwrap_or_default();
        if (host.ends_with(".slack.com") && url.path().starts_with("/archives/"))
            || host == "notion.so" || host.ends_with(".notion.so")
            || host == "notion.site" || host.ends_with(".notion.site") {
            if candidate.len() > 4096 { return Err(failure("directory URL exceeds scan budget")); }
            links.insert(candidate.to_string());
            if links.len() > 512 || links.iter().map(String::len).sum::<usize>() > 48 * 1024 {
                return Err(failure("directory links exceed request budget"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_local_candidate_survives_restart_without_http_until_source_advances() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use crate::scaffold::ScaffoldClient;

        // Either transcript progress or a registry ACK must reopen the query.
        for advancing_key in ["s:2", REGISTRY_VERSION] {
            let dir = tempfile::tempdir().unwrap();
            let id = "stale-session";
            let store = DocsStore::open(dir.path()).unwrap();
            store.queue_directory(id).unwrap();
            let generation = store.sync_job_version(id, JOB).unwrap();
            let local = Version::from([(REGISTRY_VERSION.into(), 12), ("s:1".into(), 4)]);
            let mut advanced = local.clone();
            advanced.insert(advancing_key.into(), if advancing_key == REGISTRY_VERSION { 13 } else { 1 });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = ScaffoldClient::new(format!("http://{}", listener.local_addr().unwrap()),
                "project", Arc::new(zeron_rpc::StaticToken("test".into()))).unwrap();
            let requests = Arc::new(AtomicUsize::new(0));
            let counted = requests.clone();
            let body = json!({"session": {"revision": 9, "sourceVersion": advanced, "deleted": false}}).to_string();
            let server = tokio::spawn(async move {
                for _ in 0..2 {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = [0; 8192];
                    stream.read(&mut request).await.unwrap();
                    counted.fetch_add(1, Ordering::SeqCst);
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
            });
            // Old evidence (without the new optional field) remains readable.
            let evidence: Evidence = serde_json::from_value(json!({
                "source_version": local, "confirmed": false,
            })).unwrap();
            let mut state = (evidence, BTreeMap::new());
            drain_pending(&store, || Ok(()), &mut state, |id, (mut evidence, mut required)| {
                let (store, client, local) = (&store, &client, &local);
                async move {
                    let result = lookup_remote(store, client, &id, None, local, &mut evidence, &mut required).await.map(|_| true);
                    ((evidence, required), result)
                }
            }).await.unwrap();
            assert_eq!(requests.load(Ordering::SeqCst), 1);
            assert_eq!(store.sync_job_version(id, JOB).unwrap(), generation);
            assert_eq!(state.0.source_version, local, "never borrow remote version evidence");
            assert_eq!(state.0.rejected_source_version.as_ref(), Some(&local));
            assert!(!state.0.confirmed);
            drop(store);
            state.1.clear();

            let store = DocsStore::open(dir.path()).unwrap();
            state.0 = serde_json::from_str(&store.directory_state(id).unwrap().unwrap()).unwrap();
            for _ in 0..3 {
                drain_pending(&store, || Ok(()), &mut state, |id, (mut evidence, mut required)| {
                    let (store, client, local) = (&store, &client, &local);
                    async move {
                        let result = lookup_remote(store, client, &id, None, local, &mut evidence, &mut required).await.map(|_| true);
                        ((evidence, required), result)
                    }
                }).await.unwrap();
            }
            assert_eq!(requests.load(Ordering::SeqCst), 1, "unchanged source must not query after restart");
            assert_eq!(store.sync_job_version(id, JOB).unwrap(), generation);
            assert_eq!(state.0.source_version, local);
            assert!(!state.0.confirmed);

            drain_pending(&store, || Ok(()), &mut state, |id, (mut evidence, mut required)| {
                let (store, client, advanced) = (&store, &client, &advanced);
                async move {
                    let result = lookup_remote(store, client, &id, None, advanced, &mut evidence, &mut required).await.map(|remote| {
                        assert_eq!(remote["session"]["revision"], 9);
                        // A snapshot arriving during this fresh request must survive its
                        // completion, just as it does during a normal publication.
                        store.queue_directory(&id).unwrap();
                        true
                    });
                    ((evidence, required), result)
                }
            }).await.unwrap();
            assert_eq!(requests.load(Ordering::SeqCst), 2);
            server.await.unwrap();
            assert!(state.0.rejected_source_version.is_none());
            assert_eq!(state.0.source_version, local);
            assert!(!state.0.confirmed);
            assert!(store.sync_job_version(id, JOB).unwrap().is_some());
            assert_ne!(store.sync_job_version(id, JOB).unwrap(), generation);
        }
    }

    #[tokio::test]
    async fn first_remote_throttle_retains_all_74_durable_jobs() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use crate::scaffold::ScaffoldClient;

        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let before: Vec<_> = (0..74).map(|index| format!("session-{index:02}")).collect();
        for id in &before { store.queue_directory(id).unwrap(); }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = ScaffoldClient::new(format!("http://{}", listener.local_addr().unwrap()),
            "project", Arc::new(zeron_rpc::StaticToken("test".into()))).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            stream.read(&mut request).await.unwrap();
            let body = r#"{"error":"rate_limited"}"#;
            stream.write_all(format!("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 125\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let mut attempts = 0;
        let cooldown = drain_pending(&store, || Ok(()), &mut attempts, |id, attempts| {
            let client = &client;
            async move {
                let result = client.get_crew_session(&id, None).await.map(|_| true)
                    .map_err(|source| PublishError::Remote { operation: "revision lookup", source });
                (attempts + 1, result)
            }
        }).await.unwrap();
        server.await.unwrap();
        assert_eq!(attempts, 1);
        assert_eq!(cooldown, Some(Duration::from_secs(125)));
        drop(store);
        let reopened = DocsStore::open(dir.path()).unwrap();
        let mut retained = reopened.pending_sync_jobs(JOB, "", 64).unwrap();
        retained.extend(reopened.pending_sync_jobs(JOB, retained.last().unwrap(), 64).unwrap());
        assert_eq!(retained, before);
    }

    #[tokio::test(start_paused = true)]
    async fn signal_storm_cannot_shorten_or_extend_worker_cooldown() {
        for delay in [RETRY_DELAY, Duration::from_secs(125)] {
            let (registry_tx, mut registry) = tokio::sync::watch::channel(0);
            let (snapshots_tx, mut snapshots) = tokio::sync::watch::channel(0);
            let (identity_tx, mut identity) = tokio::sync::watch::channel(AuthState::SignedOut);
            // Include the inventory's own wake already pending before waiting.
            snapshots_tx.send_replace(1);
            let waiter = tokio::spawn(async move {
                wait_for_work(&mut registry, &mut snapshots, &mut identity, Some(delay)).await
            });
            tokio::task::yield_now().await;
            for index in 1..delay.as_secs() {
                registry_tx.send_replace(index);
                snapshots_tx.send_replace(index);
                identity_tx.send_replace(AuthState::SignedOut);
                tokio::time::advance(Duration::from_secs(1)).await;
                tokio::task::yield_now().await;
                assert!(!waiter.is_finished(), "notification bypassed cooldown");
            }
            tokio::time::advance(Duration::from_secs(1)).await;
            assert_eq!(waiter.await.unwrap(), Some(true));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unrepresentable_deadline_never_overflows_or_retries_early() {
        let started = tokio::time::Instant::now();
        let waiter = tokio::spawn(wait_for_retry(started, Duration::MAX));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2 * 24 * 60 * 60)).await;
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        waiter.abort();
    }

    #[tokio::test]
    async fn row_conflict_does_not_starve_next_job_or_complete_new_generation() {
        use crate::scaffold::{ScaffoldApiError, ScaffoldApiErrorDisplay};

        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        for id in ["a-local", "b-conflict", "c-new-generation", "d-success"] {
            store.queue_directory(id).unwrap();
        }
        let mut visited = Vec::new();
        let cooldown = drain_pending(&store, || Ok(()), &mut visited, |id, mut visited| {
            let result = match id.as_str() {
                "a-local" => Err(failure("directory awaits acknowledged registry row").into()),
                "b-conflict" => Err(PublishError::Remote { operation: "upsert", source:
                    ScaffoldError::Api(ScaffoldApiErrorDisplay(ScaffoldApiError {
                        status: 409, code: "revision_conflict".into(), message: None,
                        retry_after: Some(Duration::from_secs(300)),
                    })) }),
                "c-new-generation" => { store.queue_directory(&id).unwrap(); Ok(true) },
                _ => Ok(true),
            };
            visited.push(id);
            std::future::ready((visited, result))
        }).await.unwrap();
        assert_eq!(cooldown, None);
        assert_eq!(visited, ["a-local", "b-conflict", "c-new-generation", "d-success"]);
        assert_eq!(store.pending_sync_jobs(JOB, "", 100).unwrap(),
            ["a-local", "b-conflict", "c-new-generation"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conflicted_jobs_wait_for_own_source_and_retry_transport_failures() {
        use std::{collections::VecDeque, convert::Infallible, sync::Mutex};
        use bytes::Bytes;
        use http_body_util::{BodyExt, Full};
        use hyper::{Request, Response, body::Incoming, service::service_fn};
        use hyper_util::rt::TokioIo;
        use crate::{auth::AuthConfig, scaffold::{ScaffoldClient, ScaffoldRuntime,
            UnavailableDeviceJoinGrantProvider}, workspace_host::WorkspaceHostConfig};
        use zeron_doc::{RegistryDoc, REGISTRY_DOC_ID, registry::{RegistryRow, KIND_CHATS}};
        use zeron_sync::scaffold_auth::{save_session, ScaffoldConfig, ScaffoldIdentity,
            ScaffoldSession, DEFAULT_OAUTH_SCOPES};

        let id = "9dd173da-fb53-48df-9581-026009c783e2";
        let doc = loro::LoroDoc::new();
        doc.set_peer_id(42).unwrap();
        doc.get_map("source").insert("observed", true).unwrap();
        doc.commit();
        let snapshot = doc.export(loro::ExportMode::Snapshot).unwrap();
        let (mut version, _) = project(&snapshot).unwrap();
        version.insert(REGISTRY_VERSION.into(), 9);
        let remote = json!({"session":{"revision":7, "sourceVersion":version, "deleted":false}});
        let responses = Arc::new(Mutex::new(VecDeque::from([
            (200_u16, remote.clone()), // First stale source.
            (503, json!({})), // Catch-up is not suppressed after a failed GET.
            (200, remote.clone()), (503, json!({})), // Nor after a failed upsert.
            (200, remote), (200, json!({"revision":8})),
        ])));
        let requests = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn({
            let origin = origin.clone();
            let responses = responses.clone();
            let requests = requests.clone();
            async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let origin = origin.clone();
                    let responses = responses.clone();
                    let requests = requests.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |request: Request<Incoming>| {
                            let origin = origin.clone();
                            let responses = responses.clone();
                            let requests = requests.clone();
                            async move {
                                let path = request.uri().path().to_owned();
                                let body = request.into_body().collect().await.unwrap().to_bytes();
                                let (status, response) = match path.as_str() {
                                    "/auth/session" => (200, json!({"userId":"alice", "email":"alice@example.com",
                                        "projectScope":"project-a", "deploymentId":"deployment-a",
                                        "credential":"scaffold", "capabilities":["session.read"]})),
                                    "/api/code-sandboxes/auth/session" => (200, json!({"ok":true,
                                        "resource":origin, "actor":{"sub":"alice", "auth":"iap"},
                                        "scopes":DEFAULT_OAUTH_SCOPES.split_whitespace().collect::<Vec<_>>()})),
                                    "/api/crew-directory/get" | "/api/crew-directory/upsert" => {
                                        requests.lock().unwrap().push((path, serde_json::from_slice(&body).unwrap()));
                                        responses.lock().unwrap().pop_front().unwrap_or((500, json!({})))
                                    }
                                    _ => panic!("unexpected request: {path}"),
                                };
                                Ok::<_, Infallible>(Response::builder().status(status)
                                    .header("content-type", "application/json").header("connection", "close")
                                    .body(Full::new(Bytes::from(response.to_string()))).unwrap())
                            }
                        });
                        hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service).await.unwrap();
                    });
                }
            }
        });
        let dir = tempfile::tempdir().unwrap();
        save_session(&dir.path().join("scaffold-session.json"), &ScaffoldSession {
            access_token: "sc_rc_test".into(), resource: origin.clone(), edge_origin: origin.clone(), device: None,
            identity: ScaffoldIdentity { user_id: "alice".into(), email: "alice@example.com".into(), name: None,
                project_scope: "project-a".into(), deployment_id: Some("deployment-a".into()),
                capabilities: vec!["session.read".into()] },
        }).unwrap();
        let mut config = AuthConfig::new(&origin, dir.path());
        config.scaffold = Some(ScaffoldConfig::new(&origin, &origin, "project-a"));
        let auth = Auth::detect(config).await;
        let runtime = ScaffoldRuntime::new(
            ScaffoldClient::new(&origin, "project-a", auth.for_profile_tokens()).unwrap(),
            &origin, Arc::new(UnavailableDeviceJoinGrantProvider),
        ).with_deployment_id("deployment-a".into());
        let store = Arc::new(DocsStore::open(&dir.path().join("store")).unwrap());
        let services = CrewServices::new(auth, runtime, store.clone()).unwrap();
        let mut registry = RegistryDoc::new("test-device");
        let _ = registry.apply_state(9, true, 0, vec![RegistryRow {
            kind: KIND_CHATS.into(), id: id.into(), seq: 9, deleted: false, del_hlc: None,
            fields: BTreeMap::from([("id".into(), json!(id)), ("deviceId".into(), json!("test-device")),
                ("crew".into(), serde_json::to_value(services.session_metadata(id).unwrap()).unwrap())]),
            clocks: BTreeMap::new(),
        }]);
        store.save_snapshot(REGISTRY_DOC_ID, &registry.to_bytes().unwrap()).unwrap();
        let workspace = WorkspaceHost::open(store.clone(), WorkspaceHostConfig {
            device_id: "test-device".into(), device_name: "Test".into(), platform: "linux".into(),
            org_id: "project-a".into(), user_id: "alice".into(), edge: None, local_only: false,
        }).unwrap();
        let mut required = BTreeMap::new();
        enqueue_registry(&store, &workspace).await.unwrap();
        drain(&store, &services, &workspace, &mut required).await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(required[id], version);
        let rejected: Evidence = serde_json::from_str(&store.directory_state(id).unwrap().unwrap()).unwrap();
        assert!(rejected.source_version.is_empty(), "remote counters are not evidence");
        assert_eq!(rejected.rejected_source_version, Some(Version::from([(REGISTRY_VERSION.into(), 9)])));

        for _ in 0..3 {
            // The worker inventories on any registry wake and drains on any snapshot wake.
            enqueue_registry(&store, &workspace).await.unwrap();
            store.save_snapshot("unrelated", &snapshot).unwrap();
            drain(&store, &services, &workspace, &mut required).await.unwrap();
            assert_eq!(requests.lock().unwrap().len(), 1);
            assert!(store.sync_job_version(id, JOB).unwrap().is_some());
        }
        required.clear(); // The rejected local candidate remains suppressed across restart.
        drain(&store, &services, &workspace, &mut required).await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
        store.save_snapshot(id, &snapshot).unwrap();
        for expected_requests in [2, 4] {
            drain(&store, &services, &workspace, &mut required).await.unwrap();
            assert_eq!(requests.lock().unwrap().len(), expected_requests);
            assert!(store.sync_job_version(id, JOB).unwrap().is_some());
        }
        drain(&store, &services, &workspace, &mut required).await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 6);
        assert!(responses.lock().unwrap().is_empty());
        assert!(store.sync_job_version(id, JOB).unwrap().is_none());
        assert!(required.is_empty());
        {
            let requests = requests.lock().unwrap();
            let published = &requests[5];
            assert_eq!(published.0, "/api/crew-directory/upsert");
            assert_eq!(published.1["sourceVersion"], json!(version));
            assert_eq!(published.1["expectedRevision"], 7);
        }
        let saved = store.directory_state(id).unwrap().unwrap();
        let evidence: Evidence = serde_json::from_str(&saved).unwrap();
        assert!(evidence.confirmed);
        assert_eq!(evidence.source_version, version);

        // Losing the local snapshot must wait on saved evidence without any HTTP.
        store.delete_snapshot(id).unwrap();
        store.queue_directory(id).unwrap();
        for _ in 0..2 {
            drain(&store, &services, &workspace, &mut required).await.unwrap();
            assert!(store.sync_job_version(id, JOB).unwrap().is_some());
            assert_eq!(requests.lock().unwrap().len(), 6);
        }
        store.save_snapshot(id, &snapshot).unwrap();
        drain(&store, &services, &workspace, &mut required).await.unwrap();
        assert!(store.sync_job_version(id, JOB).unwrap().is_none());
        assert_eq!(requests.lock().unwrap().len(), 6);
        assert_eq!(store.directory_state(id).unwrap().unwrap(), saved);
        workspace.shutdown();
        server.abort();
    }

    #[test]
    fn stale_concurrent_and_reset_sources_never_borrow_server_evidence() {
        let old = BTreeMap::from([(REGISTRY_VERSION.into(), 9), ("s:1".into(), 4)]);
        let mut current = old.clone();
        current.insert(REGISTRY_VERSION.into(), 10);
        assert!(require_observed(&current, &old).is_ok());
        let unseen = BTreeMap::from([(REGISTRY_VERSION.into(), 11), ("s:2".into(), 1)]);
        assert!(require_observed(&current, &unseen).is_err());
        assert!(!current.contains_key("s:2"));
        current.insert(REGISTRY_VERSION.into(), 8);
        assert!(require_observed(&current, &old).unwrap_err().to_string().contains("reconciliation"));
    }

    #[test]
    fn snapshot_projection_scans_chunk_boundaries_and_excludes_private_reasoning() {
        let doc = loro::LoroDoc::new();
        doc.set_peer_id(42).unwrap();
        let row = doc.get_list("messages").push_container(loro::LoroMap::new()).unwrap();
        row.insert("role", "user").unwrap();
        let parts = row.insert_container("parts", loro::LoroList::new()).unwrap();
        let part = parts.push_container(loro::LoroMap::new()).unwrap();
        part.insert("kind", "text").unwrap();
        let text = part.insert_container("text", loro::LoroText::new()).unwrap();
        let url = "https://acme.slack.com/archives/C123/p1234567890123456?thread_ts=1234567890.123456";
        text.insert(0, &format!("{} {url}).", "x".repeat(65530))).unwrap();
        let private = parts.push_container(loro::LoroMap::new()).unwrap();
        private.insert("kind", "reasoning").unwrap();
        private.insert_container("reasoning", loro::LoroText::new()).unwrap()
            .insert(0, "https://notion.so/12345678901234567890123456789012").unwrap();
        doc.commit();
        let bytes = doc.export(loro::ExportMode::Snapshot).unwrap();
        let (version, links) = project(&bytes).unwrap();
        assert_eq!(links, vec![url]);
        assert_eq!(version["s:42"], *doc.oplog_vv().get(&42).unwrap() as u64);
    }

    #[test]
    fn pending_projection_and_observed_fence_survive_restart_and_snapshot_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9dd173da-fb53-48df-9581-026009c783e2";
        let store = DocsStore::open(dir.path()).unwrap();
        store.save_snapshot(id, b"first").unwrap();
        let first = store.sync_job_version(id, JOB).unwrap().unwrap();
        let evidence = Evidence { source_version: BTreeMap::from([(REGISTRY_VERSION.into(), 12)]), confirmed: false,
            ..Evidence::default() };
        store.save_directory_state(id, &serde_json::to_string(&evidence).unwrap()).unwrap();
        store.save_snapshot(id, b"second").unwrap();
        store.complete_sync_job(id, JOB, first).unwrap();
        assert!(store.sync_job_version(id, JOB).unwrap().is_some());
        store.delete_snapshot(id).unwrap();
        drop(store);
        let store = DocsStore::open(dir.path()).unwrap();
        assert_eq!(store.pending_sync_jobs(JOB, "", 64).unwrap(), vec![id]);
        let saved: Evidence = serde_json::from_str(&store.directory_state(id).unwrap().unwrap()).unwrap();
        assert_eq!(saved.source_version, evidence.source_version);
        assert!(!saved.confirmed, "lost ACK remains retryable, never a new session");
    }
}
