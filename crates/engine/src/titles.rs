//! Durable, coalesced session discovery. No harness runs or branch renames.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use comet_doc::{MessagePart, MessageRole, MessageStatus, SessionDoc};
use comet_sync::DocsStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{EngineError, scaffold::ScaffoldClient, workspace_host::WorkspaceHost};

type Version = BTreeMap<String, u64>;

#[derive(Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Pending {
    source_version: Version,
    revision: Option<u64>,
    deployment_id: Option<String>,
    generated_for: Option<String>,
    generated_title: Option<String>,
    next_title_at: i64,
}

struct Projection {
    source_version: Version,
    completed: Option<String>,
    input: String,
    links: Vec<String>,
}

struct Inner {
    workspace: WorkspaceHost,
    repos: crate::Repos,
    store: Arc<DocsStore>,
    client: OnceLock<ScaffoldClient>,
}

#[derive(Clone)]
pub struct TitleGenerator {
    inner: Arc<Inner>,
}

fn failure(message: &str) -> EngineError {
    EngineError::Other(message.into())
}

impl TitleGenerator {
    pub fn new(workspace: WorkspaceHost, store: Arc<DocsStore>, repos: crate::Repos) -> Self {
        Self {
            inner: Arc::new(Inner {
                workspace,
                store,
                repos,
                client: OnceLock::new(),
            }),
        }
    }

    /// Preserve initial prompt-based branch naming, independently of model titles.
    /// A durable once marker prevents refresh/restart from renaming the branch again.
    pub fn name_initial_branch(&self, id: &str, prompt: &str) {
        let this = self.clone();
        let id = id.to_string();
        let title = title_from_prompt(prompt);
        tokio::spawn(async move {
            let result = async {
                let marker = format!("crew-initial-branch:{id}");
                if title.is_empty() || this.inner.store.is_local_migration_applied(&marker)? {
                    return Ok::<(), EngineError>(());
                }
                let Some(chat) = this.inner.workspace.doc().chat(&id)? else {
                    return Ok(());
                };
                let (Some(cwd), Some(branch)) = (chat.cwd, chat.branch) else {
                    return Ok(());
                };
                if !branch.starts_with("comet/") {
                    return Ok(());
                }
                this.inner.store.mark_local_migration_applied(&marker)?;
                let renamed = this
                    .inner
                    .repos
                    .rename_worktree_branch(std::path::Path::new(&cwd), &branch, &title)
                    .await?;
                this.inner
                    .workspace
                    .compare_and_set_chat_branch(&id, Some(&branch), &renamed)?;
                Ok(())
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(session = %id, %error, "initial branch naming failed");
            }
        });
    }

    pub fn set_client(&self, client: ScaffoldClient) {
        if self.inner.client.set(client).is_err() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let mut changed = self.inner.store.watch_directory();
        tokio::spawn(async move {
            // One bounded worker: at most one model call (below the max-two budget).
            // Persistent jobs coalesce every update by session and survive process loss.
            let mut after = String::new();
            loop {
                let Some(inner) = weak.upgrade() else { return };
                let ids = match inner.store.snapshot_ids_after(&after) {
                    Ok(ids) => ids,
                    Err(error) => {
                        tracing::warn!(%error, "directory backfill inventory failed");
                        break;
                    }
                };
                if ids.is_empty() {
                    break;
                }
                for id in ids {
                    after.clone_from(&id);
                    if eligible(&inner.workspace, &id) {
                        if let Err(error) = inner.store.queue_directory(&id, false) {
                            tracing::warn!(%error, "directory backfill enqueue failed");
                        }
                    }
                }
                drop(inner);
                tokio::task::yield_now().await;
            }
            loop {
                let Some(inner) = weak.upgrade() else { return };
                match inner.store.claim_directory() {
                    Ok(Some((id, generation, state, deleted))) => {
                        let this = Self { inner };
                        let result = this.process(&id, &state, deleted).await;
                        if let Err(error) = &result {
                            tracing::warn!(session = %id, %error, "directory update retained for retry");
                        }
                        let acknowledgement = match result {
                            Ok(Some(deadline)) => this
                                .inner
                                .store
                                .defer_directory_title(&id, generation, deadline),
                            result => {
                                this.inner
                                    .store
                                    .settle_directory(&id, generation, result.is_ok())
                            }
                        };
                        if let Err(error) = acknowledgement {
                            tracing::warn!(%error, "directory acknowledgement persistence failed");
                        }
                    }
                    Ok(None) => {
                        changed.borrow_and_update();
                        let delay = inner.store.directory_retry_delay().ok().flatten();
                        drop(inner);
                        match delay {
                            Some(delay) => tokio::select! {
                                _ = tokio::time::sleep(delay) => {},
                                result = changed.changed() => { if result.is_err() { return; } },
                            },
                            None => {
                                if changed.changed().await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "directory outbox read failed");
                        drop(inner);
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                }
            }
        });
    }

    /// Snapshot/export/SQLite work happens off the turn delivery path.
    pub fn completed(&self, chat_id: String, host: crate::DocHost) {
        tokio::task::spawn_blocking(move || {
            if let Err(error) = host.persist_completed_turn(&chat_id) {
                tracing::warn!(session = %chat_id, %error, "completed turn metadata persistence failed");
            }
        });
    }

    async fn process(
        &self,
        id: &str,
        saved: &str,
        deleted: bool,
    ) -> Result<Option<i64>, EngineError> {
        if !deleted && !eligible(&self.inner.workspace, id) {
            return Ok(None);
        }
        if uuid::Uuid::parse_str(id).is_err() {
            return Ok(None);
        }
        let client = self
            .inner
            .client
            .get()
            .ok_or_else(|| failure("directory client unavailable"))?;
        let mut pending: Pending =
            serde_json::from_str(saved).map_err(|_| failure("invalid directory pending state"))?;
        let projection = if deleted {
            None
        } else {
            let bytes = self
                .inner
                .store
                .load_bounded_snapshot(id, 32 * 1024 * 1024)?
                .ok_or_else(|| failure("directory source snapshot unavailable"))?;
            Some(
                tokio::task::spawn_blocking(move || project(&bytes))
                    .await
                    .map_err(|_| failure("directory projection failed"))??,
            )
        };
        let mut title = String::new();
        if let Some(projection) = &projection {
            let metadata =
                title_metadata(&self.inner.workspace, id, projection.source_version.clone())?;
            title = metadata.0;
            pending.source_version = metadata.1;
            pending.deployment_id = self.inner.workspace.directory_deployment(id);
        } else {
            pending.source_version =
                with_workspace_version(&self.inner.workspace, pending.source_version.clone())?;
        }
        let remote = client
            .get_crew_session(id, pending.deployment_id.as_deref())
            .await
            .map_err(|_| failure("directory revision lookup failed"))?;
        if let Some(session) = remote.get("session").filter(|session| !session.is_null()) {
            if session.get("deleted").and_then(Value::as_bool) == Some(true) {
                return Ok(None);
            }
            let version: Version = serde_json::from_value(session["sourceVersion"].clone())
                .map_err(|_| failure("invalid server source version"))?;
            // Deletion is intentional and irreversible; when purged locally use the last
            // acknowledged source. Never adopt a server source to overwrite newer work.
            if !dominates(&pending.source_version, &version) {
                return Err(failure(
                    "directory source is older or concurrent; awaiting synchronized snapshot",
                ));
            }
            pending.revision = session.get("revision").and_then(Value::as_u64);
        } else {
            pending.revision = None;
        }
        self.save(id, &pending, deleted)?;
        let links = projection
            .as_ref()
            .map(|p| p.links.clone())
            .unwrap_or_default();
        let mut body = json!({"sessionId": id, "expectedRevision": pending.revision,
            "sourceVersion": pending.source_version, "generatedTitle": null,
            "title": title.chars().take(140).collect::<String>(), "links": links, "deleted": deleted});
        body["deploymentId"] = json!(pending.deployment_id);
        // Link indexing never depends on the model succeeding.
        let ack = client
            .upsert_crew_session(&body)
            .await
            .map_err(|_| failure("directory metadata update failed"))?;
        pending.revision = Some(
            ack["revision"]
                .as_u64()
                .ok_or_else(|| failure("invalid directory revision"))?,
        );
        self.save(id, &pending, deleted)?;
        let Some(projection) = projection else {
            return Ok(None);
        };
        let Some(completed) = projection.completed else {
            return Ok(None);
        };
        if pending.generated_for.as_deref() == Some(completed.as_str())
            && pending.generated_title.is_none()
        {
            return Ok(None);
        }
        let generated = if pending.generated_for.as_deref() == Some(completed.as_str()) {
            pending
                .generated_title
                .clone()
                .ok_or_else(|| failure("missing pending generated title"))?
        } else {
            let now = crate::now_ms();
            if now < pending.next_title_at {
                return Ok(Some(pending.next_title_at));
            }
            pending.next_title_at = now + 60_000;
            self.save(id, &pending, false)?;
            let previous = self
                .inner
                .workspace
                .doc()
                .generated_chat_title(id)
                .unwrap_or_default();
            let input = format!(
                "Previous generated title (retain unless the subject changed): {previous}\n{}",
                projection.input
            );
            tokio::time::timeout(
                Duration::from_secs(60),
                client.generate_crew_title(id, pending.deployment_id.as_deref(), &input),
            )
            .await
            .map_err(|_| failure("title inference timed out"))?
            .map_err(|_| failure("title inference failed"))?
        };
        pending.generated_for = Some(completed.clone());
        pending.generated_title = Some(generated.clone());
        self.save(id, &pending, false)?;
        // A manual rename during inference wins through the synchronized provenance check.
        // A newer source arriving during inference must be processed instead of published.
        let latest = self
            .inner
            .store
            .load_bounded_snapshot(id, 32 * 1024 * 1024)?
            .ok_or_else(|| failure("title source was deleted"))?;
        let current = source_version(&latest)?;
        if current != projection.source_version {
            return Err(failure("title source changed during inference"));
        }
        if self.inner.workspace.doc().chat(id)?.is_none() {
            return Ok(None);
        }
        self.inner.workspace.set_generated_title(id, &generated)?;
        self.inner.workspace.flush();
        let (effective, publication_version) = title_metadata(&self.inner.workspace, id, current)?;
        body["expectedRevision"] = json!(pending.revision);
        body["generatedTitle"] = json!(generated);
        body["title"] = json!(effective.chars().take(140).collect::<String>());
        pending.source_version = publication_version;
        body["sourceVersion"] = json!(pending.source_version);
        let ack = client
            .upsert_crew_session(&body)
            .await
            .map_err(|_| failure("directory title update failed"))?;
        pending.revision = Some(
            ack["revision"]
                .as_u64()
                .ok_or_else(|| failure("invalid directory revision"))?,
        );
        pending.generated_for = Some(completed);
        pending.generated_title = None;
        self.save(id, &pending, false)?;
        Ok(None)
    }

    fn save(&self, id: &str, state: &Pending, deleted: bool) -> Result<(), EngineError> {
        let json = serde_json::to_string(state).map_err(|_| failure("invalid directory state"))?;
        self.inner.store.save_directory_state(id, &json, deleted)?;
        Ok(())
    }
}

fn eligible(workspace: &WorkspaceHost, id: &str) -> bool {
    workspace.directory_eligible(id)
}
pub fn title_from_prompt(prompt: &str) -> String {
    prompt
        .split_whitespace()
        .take(7)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(48)
        .collect()
}

pub(crate) fn queue_deletion(
    store: &DocsStore,
    workspace: &WorkspaceHost,
    id: &str,
) -> Result<(), EngineError> {
    let source = match store.load_bounded_snapshot(id, 32 * 1024 * 1024)? {
        Some(bytes) => source_version(&bytes)?,
        None => Version::new(),
    };
    let pending = Pending {
        source_version: with_workspace_version(workspace, source)?,
        deployment_id: workspace.directory_deployment(id),
        ..Pending::default()
    };
    store.queue_directory_deletion(
        id,
        &serde_json::to_string(&pending).map_err(|_| failure("invalid deletion source"))?,
    )?;
    Ok(())
}

fn dominates(candidate: &Version, previous: &Version) -> bool {
    previous
        .iter()
        .all(|(peer, counter)| candidate.get(peer).copied().unwrap_or(0) >= *counter)
}

fn with_workspace_version(
    workspace: &WorkspaceHost,
    mut source: Version,
) -> Result<Version, EngineError> {
    source.extend(
        workspace
            .doc()
            .doc()
            .oplog_vv()
            .iter()
            .map(|(peer, counter)| (format!("w:{peer}"), *counter as u64)),
    );
    if source.len() > 128 {
        return Err(failure("directory source version exceeds 128 peers"));
    }
    Ok(source)
}

fn title_metadata(
    workspace: &WorkspaceHost,
    id: &str,
    source: Version,
) -> Result<(String, Version), EngineError> {
    for _ in 0..3 {
        let before = workspace.doc().doc().oplog_vv();
        let title = workspace
            .doc()
            .chat(id)?
            .and_then(|chat| chat.title)
            .unwrap_or_default();
        let version = with_workspace_version(workspace, source.clone())?;
        if workspace.doc().doc().oplog_vv() == before {
            return Ok((title, version));
        }
    }
    Err(failure("workspace metadata changed during projection"))
}

fn version(doc: &SessionDoc) -> Result<Version, EngineError> {
    let version: Version = doc
        .doc()
        .oplog_vv()
        .iter()
        .map(|(peer, counter)| (format!("s:{peer}"), *counter as u64))
        .collect();
    if version.len() > 128 {
        return Err(failure("directory source version exceeds 128 peers"));
    }
    Ok(version)
}

fn source_version(bytes: &[u8]) -> Result<Version, EngineError> {
    let raw = loro::LoroDoc::new();
    raw.import(bytes)
        .map_err(|_| failure("invalid directory source snapshot"))?;
    version(&SessionDoc::from_doc(raw))
}

fn project(bytes: &[u8]) -> Result<Projection, EngineError> {
    let raw = loro::LoroDoc::new();
    raw.import(bytes)
        .map_err(|_| failure("invalid directory source snapshot"))?;
    let doc = SessionDoc::from_doc(raw);
    let completed_marker = match doc.doc().get_map("meta").get("directoryCompletedTurn") {
        Some(loro::ValueOrContainer::Value(loro::LoroValue::String(value))) => {
            Some(value.to_string())
        }
        _ => None,
    };
    let mut links = BTreeSet::new();
    let mut initial = String::new();
    let mut recent = String::new();
    let mut turn = String::new();
    let mut completed = None;
    for index in 0..doc.directory_entry_count() {
        let Some(entry) = doc.directory_entry(index)? else {
            continue;
        };
        let mut text = String::new();
        let mut carry = String::new();
        doc.directory_visit_text(index, |chunk, finished| {
            scan_chunk(&mut carry, chunk, finished, &mut links).map_err(|_| {
                comet_doc::DocError::Schema("directory link scan exceeded bounds".into())
            })?;
            if entry.role == MessageRole::User {
                append_bounded(&mut text, chunk, 6000);
            }
            Ok(())
        })?;
        for part in &entry.parts {
            match part {
                MessagePart::Text { text: value, .. }
                | MessagePart::TextWindow { text: value, .. } => {
                    if entry.role != MessageRole::User {
                        append_bounded(&mut text, value, 6000);
                    }
                }
                MessagePart::Tool { call, .. } => {
                    let value =
                        serde_json::to_value(call).map_err(|_| failure("invalid tool content"))?;
                    tool_text(&value, &mut links, &mut text)?;
                }
                _ => {}
            }
        }
        if initial.is_empty() && entry.role == MessageRole::User && entry.peer_message.is_none() {
            append_bounded(&mut initial, &text, 6000);
        }
        if entry.role == MessageRole::User && entry.peer_message.is_none() {
            turn.clear();
            append_bounded(&mut turn, &text, 6000);
        } else if entry.role == MessageRole::Assistant {
            turn.push_str(&text);
            if turn.len() > 12000 {
                let mut start = turn.len() - 12000;
                while !turn.is_char_boundary(start) {
                    start += 1;
                }
                turn.drain(..start);
            }
        }
        if entry.role == MessageRole::Assistant
            && entry.status == Some(MessageStatus::Complete)
            && completed_marker
                .as_deref()
                .is_none_or(|marker| marker == entry.id)
            && !entry
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::Error { .. }))
        {
            completed = Some(entry.id);
            recent.clone_from(&turn);
        }
    }
    Ok(Projection {
        source_version: version(&doc)?,
        completed,
        input: format!(
            "Initial request:\n{initial}\nRecent completed response and tools:\n{recent}"
        ),
        links: links.into_iter().collect(),
    })
}

fn append_bounded(out: &mut String, text: &str, budget: usize) {
    for ch in text.chars() {
        if out.len() + ch.len_utf8() > budget {
            break;
        }
        out.push(ch);
    }
    if out.len() < budget {
        out.push('\n');
    }
}

fn tool_text(
    value: &Value,
    links: &mut BTreeSet<String>,
    text: &mut String,
) -> Result<(), EngineError> {
    match value {
        Value::String(value) => {
            extract_links(value, links)?;
            append_bounded(text, value, 6000);
        }
        Value::Array(items) => {
            for item in items {
                tool_text(item, links, text)?;
            }
        }
        Value::Object(fields) => {
            for (key, item) in fields {
                if !matches!(
                    key.as_str(),
                    "reasoning" | "thinking" | "system" | "systemPrompt"
                ) {
                    tool_text(item, links, text)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn url_separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '|' | ')' | ']')
}

fn scan_chunk(
    carry: &mut String,
    chunk: &str,
    finished: bool,
    links: &mut BTreeSet<String>,
) -> Result<(), EngineError> {
    carry.push_str(chunk);
    let split = if finished {
        carry.len()
    } else {
        carry
            .char_indices()
            .rev()
            .find(|(_, ch)| url_separator(*ch))
            .map_or(0, |(index, ch)| index + ch.len_utf8())
    };
    extract_links(&carry[..split], links)?;
    carry.drain(..split);
    if carry.len() > 4096 {
        // Parsing the token is bounded by one 64KiB chunk; only supported
        // source URLs may fail the link budget, not data URLs or unrelated sites.
        extract_links(carry, links)?;
        let mut suffix = carry.len().saturating_sub(8);
        while !carry.is_char_boundary(suffix) {
            suffix += 1;
        }
        carry.drain(..suffix);
    }
    Ok(())
}

fn extract_links(text: &str, links: &mut BTreeSet<String>) -> Result<(), EngineError> {
    for (start, _) in text
        .match_indices("https://")
        .chain(text.match_indices("http://"))
    {
        let candidate = text[start..]
            .split(|ch: char| {
                ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '|' | ')' | ']')
            })
            .next()
            .unwrap_or_default()
            .trim_end_matches(['.', ',', ';', ':']);
        let Ok(url) = reqwest::Url::parse(candidate) else {
            continue;
        };
        let host = url.host_str().unwrap_or_default();
        if (host.ends_with(".slack.com") && url.path().starts_with("/archives/"))
            || host == "notion.so"
            || host.ends_with(".notion.so")
            || host == "notion.site"
            || host.ends_with(".notion.site")
        {
            if candidate.len() > 4096 {
                return Err(failure("directory URL exceeds scan budget"));
            }
            links.insert(candidate.to_string());
            // Server owns canonical-key deduplication; request bytes are the local bound.
            if links.iter().map(String::len).sum::<usize>() > 48 * 1024 {
                return Err(failure("directory links exceed request budget"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_versions_reject_stale_and_concurrent_sources() {
        let old = BTreeMap::from([("1".into(), 3)]);
        assert!(dominates(&old, &old));
        assert!(!dominates(&BTreeMap::from([("2".into(), 4)]), &old));
        assert!(!dominates(&BTreeMap::from([("1".into(), 2)]), &old));
        assert!(dominates(
            &BTreeMap::from([("1".into(), 4), ("2".into(), 1)]),
            &old
        ));
    }
    #[test]
    fn links_are_extracted_without_model_or_unrelated_urls() {
        let mut links = BTreeSet::new();
        extract_links("See <https://team.slack.com/archives/C123/p1234567890123456?thread_ts=1234567890.123456|thread> and https://www.notion.so/abc-12345678901234567890123456789012. https://example.org/private", &mut links).unwrap();
        assert_eq!(links, BTreeSet::from([
            "https://team.slack.com/archives/C123/p1234567890123456?thread_ts=1234567890.123456".to_string(),
            "https://www.notion.so/abc-12345678901234567890123456789012".to_string(),
        ]));
    }

    #[tokio::test]
    async fn persisted_completion_drives_transport_and_manual_rename_wins() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let id = "10000000-0000-4000-8000-000000000001";
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            crate::WorkspaceHostConfig {
                device_id: "device-a".into(),
                device_name: "test".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "user-a".into(),
                edge: None,
            },
        )
        .unwrap();
        workspace.claim_chat(id, None).unwrap();
        let doc = SessionDoc::init(id).unwrap();
        let entry = |id: &str, role, text: &str| comet_doc::SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: "text".into(),
                text: text.into(),
            }],
            created_at: 1,
            device_id: "device-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            peer_message: None,
        };
        doc.push_message(&entry(
            "user",
            MessageRole::User,
            "Investigate https://team.slack.com/archives/C123/p1234567890123456",
        ))
        .unwrap();
        store
            .save_snapshot(id, &doc.export_snapshot().unwrap())
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = ScaffoldClient::new(
            format!("http://{}", listener.local_addr().unwrap()),
            "project-a",
            Arc::new(comet_rpc::StaticToken("test-user".into())),
        )
        .unwrap();
        let (initial_tx, initial_rx) = tokio::sync::oneshot::channel();
        let (generated_tx, generated_rx) = tokio::sync::oneshot::channel();
        let target = workspace.clone();
        let server = tokio::spawn(async move {
            let mut initial_tx = Some(initial_tx);
            let mut generated_tx = Some(generated_tx);
            let mut revision = 0;
            let mut source = json!({});
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let (end, size) = loop {
                    let mut chunk = [0u8; 4096];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|chunk| chunk == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let size: usize = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        break (end + 4, size);
                    }
                };
                while bytes.len() < end + size {
                    let mut chunk = [0u8; 4096];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let headers = String::from_utf8_lossy(&bytes[..end]);
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("authorization: bearer test-user")
                );
                let body: Value = serde_json::from_slice(&bytes[end..end + size]).unwrap();
                let response = if headers.starts_with("POST /api/crew-directory/get ") {
                    if revision == 0 {
                        json!({"session": null})
                    } else {
                        json!({"session": {"sessionId": id, "revision": revision, "sourceVersion": source, "deleted": false}})
                    }
                } else if headers.starts_with("POST /api/crew-directory/title ") {
                    assert!(
                        initial_tx.is_none(),
                        "model cannot precede persisted metadata"
                    );
                    assert!(
                        body["input"]
                            .as_str()
                            .unwrap()
                            .contains("Investigation completed")
                    );
                    target.rename_chat(id, "My manual title").unwrap();
                    json!({"title": "Generated incident investigation"})
                } else {
                    assert!(headers.starts_with("POST /api/crew-directory/upsert "));
                    assert_eq!(
                        body["expectedRevision"],
                        if revision == 0 {
                            Value::Null
                        } else {
                            json!(revision)
                        }
                    );
                    revision += 1;
                    source = body["sourceVersion"].clone();
                    if let Some(tx) = initial_tx.take() {
                        tx.send(()).unwrap();
                    }
                    if body["generatedTitle"].is_string() {
                        assert_eq!(body["title"], "My manual title");
                        assert!(
                            source
                                .as_object()
                                .unwrap()
                                .keys()
                                .any(|key| key.starts_with("w:"))
                        );
                        if let Some(tx) = generated_tx.take() {
                            tx.send(body.clone()).unwrap();
                        }
                    }
                    json!({"revision": revision})
                };
                let response = response.to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
        });
        let titles = TitleGenerator::new(
            workspace.clone(),
            store.clone(),
            crate::Repos::new(dir.path(), "device-a"),
        );
        titles.set_client(client);
        tokio::time::timeout(Duration::from_secs(5), initial_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(workspace.doc().generated_chat_title(id).is_none());
        doc.push_message(&entry(
            "assistant",
            MessageRole::Assistant,
            "Investigation completed",
        ))
        .unwrap();
        doc.doc()
            .get_map("meta")
            .insert("directoryCompletedTurn", "assistant")
            .unwrap();
        doc.doc().commit();
        store
            .save_snapshot(id, &doc.export_snapshot().unwrap())
            .unwrap();
        store.queue_directory(id, false).unwrap();
        let uploaded = tokio::time::timeout(Duration::from_secs(5), generated_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            uploaded["links"][0],
            "https://team.slack.com/archives/C123/p1234567890123456"
        );
        assert_eq!(
            workspace.doc().chat(id).unwrap().unwrap().title.as_deref(),
            Some("My manual title")
        );
        server.abort();
    }
}
