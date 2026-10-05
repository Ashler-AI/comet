//! Replaceable Loro cache binding. Durable semantic intents do not depend on Loro ancestry.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use loro::{EventTriggerKind, ExportMode, Index, LoroDoc, LoroList, LoroMap, LoroValue, ToJson};
use loro::event::{Diff, DiffEvent, ListDiffItem};
use comet_proto::PublicationRecord;
use serde::{Deserialize, Serialize};
use crate::{DocError, SessionDoc, SessionCommandEntry, SessionCommandStatus, SessionMessageEntry};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingRecord {
    pub container: String,
    pub key: String,
    pub before: Option<serde_json::Value>,
    pub value: Option<serde_json::Value>,
    #[serde(default)]
    pub version: Vec<u8>,
    #[serde(default)]
    pub acknowledged: bool,
}
type Persist = Arc<dyn Fn(&[Arc<PendingRecord>], Option<&[u8]>) -> Result<(), String> + Send + Sync>;
type RootCallback = loro::event::Subscriber;
type LocalCallback = Arc<dyn Fn(&Vec<u8>) + Send + Sync>;
struct Observer { root: Option<RootCallback>, local: Option<LocalCallback>, subscription: loro::Subscription }
struct State {
    raw: LoroDoc,
    observers: BTreeMap<u64, Observer>,
    next_observer: u64,
    journal: Option<loro::Subscription>,
}
struct Inner {
    state: Mutex<State>,
    gate: Mutex<()>,
    pending: Mutex<BTreeMap<(String, String), Arc<PendingRecord>>>,
    baseline: Mutex<BTreeMap<(String, String), serde_json::Value>>,
    persist: Mutex<Option<Persist>>,
    error: Mutex<Option<String>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    scope_generation: std::sync::atomic::AtomicU64,
}
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> { value.lock().unwrap_or_else(PoisonError::into_inner) }
#[derive(Clone)]
pub struct SharedDocument(Arc<Inner>);
pub struct DocumentSubscription { inner: Weak<Inner>, id: u64 }
impl Drop for DocumentSubscription {
    fn drop(&mut self) { if let Some(inner) = self.inner.upgrade() { lock(&inner.state).observers.remove(&self.id); } }
}
impl SharedDocument {
    pub fn new(raw: LoroDoc) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State { raw, observers: BTreeMap::new(), next_observer: 0, journal: None }),
            gate: Mutex::new(()), pending: Mutex::new(BTreeMap::new()), baseline: Mutex::new(BTreeMap::new()),
            persist: Mutex::new(None), error: Mutex::new(None),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scope_generation: std::sync::atomic::AtomicU64::new(0),
        }))
    }
    pub fn raw(&self) -> LoroDoc { lock(&self.0.state).raw.clone() }
    pub fn generation(&self) -> u64 { self.0.generation.load(std::sync::atomic::Ordering::Acquire) }
    pub fn scope_generation(&self) -> u64 { self.0.scope_generation.load(std::sync::atomic::Ordering::Acquire) }
    /// Called only while the application holds operation() for an empty retarget.
    pub fn invalidate_scope(&self) {
        self.0.scope_generation.fetch_add(1,std::sync::atomic::Ordering::Release);
        self.0.generation.fetch_add(1,std::sync::atomic::Ordering::Release);
    }
    pub fn generation_clock(&self) -> Arc<std::sync::atomic::AtomicU64> { self.0.generation.clone() }
    pub fn oplog_vv(&self) -> loro::VersionVector { self.raw().oplog_vv() }
    pub fn state_vv(&self) -> loro::VersionVector { self.raw().state_vv() }
    pub fn get_deep_value(&self) -> loro::LoroValue { self.raw().get_deep_value() }
    pub fn shallow_since_vv(&self) -> loro::ImVersionVector { self.raw().shallow_since_vv() }
    pub fn is_detached(&self) -> bool { self.raw().is_detached() }
    pub fn recovery_error(&self) -> Option<String> { lock(&self.0.error).clone() }
    pub fn fail_recovery(&self, error: String) { *lock(&self.0.error) = Some(error); }
    pub fn has_journal(&self) -> bool { lock(&self.0.persist).is_some() }
    pub fn import(&self, bytes: &[u8]) -> Result<loro::ImportStatus, loro::LoroError> {
        let _operation = self.operation();
        self.raw().import(bytes)
    }
    pub fn import_scoped(&self, scope_generation: u64, bytes: &[u8]) -> Result<loro::ImportStatus, DocError> {
        let _operation = self.operation();
        if scope_generation != self.scope_generation() { return Err(DocError::Schema("retired room scope cannot import into the live binding".into())); }
        self.raw().import(bytes).map_err(|error| DocError::Schema(error.to_string()))
    }
    pub fn operation(&self) -> MutexGuard<'_, ()> { lock(&self.0.gate) }
    pub fn get_map(&self, name: &str) -> LoroMap { self.raw().get_map(name) }
    pub fn get_list(&self, name: &str) -> LoroList { self.raw().get_list(name) }
    pub fn export(&self, mode: ExportMode<'_>) -> Result<Vec<u8>, loro::LoroEncodeError> { self.raw().export(mode) }
    pub fn commit(&self) -> Result<(), DocError> {
        self.raw().commit();
        if let Some(error) = lock(&self.0.error).as_ref() { return Err(DocError::Schema(error.clone())); }
        Ok(())
    }
    pub fn subscribe_root(&self, callback: RootCallback) -> DocumentSubscription {
        let mut state = lock(&self.0.state);
        let subscription = state.raw.subscribe_root(callback.clone());
        let id = state.next_observer; state.next_observer += 1;
        state.observers.insert(id, Observer { root: Some(callback), local: None, subscription });
        DocumentSubscription { inner: Arc::downgrade(&self.0), id }
    }
    pub fn subscribe_local_update(&self, callback: LocalCallback) -> DocumentSubscription {
        let mut state = lock(&self.0.state);
        let cb = callback.clone();
        let subscription = state.raw.subscribe_local_update(Box::new(move |bytes| { cb(bytes); true }));
        let id = state.next_observer; state.next_observer += 1;
        state.observers.insert(id, Observer { root: None, local: Some(callback), subscription });
        DocumentSubscription { inner: Arc::downgrade(&self.0), id }
    }
    /// Install the identity-local semantic journal before accepting mutations. Existing
    /// cached records are migrated once by the caller, never discarded on recovery.
    pub fn install_journal(&self, records: Vec<PendingRecord>, persist: Persist) {
        for record in records { if !record.acknowledged { lock(&self.0.pending).insert((record.container.clone(), record.key.clone()), Arc::new(record)); } }
        *lock(&self.0.persist) = Some(persist);
        let raw = self.raw();
        let baseline = map_rows(&raw);
        *lock(&self.0.baseline) = baseline;
        let subscription = journal_subscription(&self.0, &raw);
        lock(&self.0.state).journal = Some(subscription);
    }
    pub fn pending_records(&self) -> Vec<PendingRecord> { lock(&self.0.pending).values().map(|record| record.as_ref().clone()).collect() }
    /// Remove only intents covered by an authenticated server acknowledgement.
    /// Completed command outcomes remain on disk after leaving the hot outbox.
    pub fn acknowledge(&self, version: &loro::VersionVector) -> Result<(), DocError> {
        self.acknowledge_scoped(self.scope_generation(),version)
    }
    pub fn acknowledge_scoped(&self, scope_generation: u64, version: &loro::VersionVector) -> Result<(), DocError> {
        let _operation = self.operation();
        if scope_generation != self.scope_generation() { return Err(DocError::Schema("retired room scope cannot acknowledge current intents".into())); }
        let acknowledged: Vec<_> = lock(&self.0.pending).values().filter(|record| {
            !record.version.is_empty() && loro::VersionVector::decode(&record.version)
                .is_ok_and(|vv| version.includes_vv(&vv))
        }).map(|record| { let mut record = record.as_ref().clone(); record.acknowledged = true; Arc::new(record) }).collect();
        if acknowledged.is_empty() { return Ok(()) }
        if let Some(persist) = lock(&self.0.persist).as_ref() { persist(&acknowledged, None).map_err(DocError::Schema)?; }
        let mut pending = lock(&self.0.pending);
        for record in acknowledged { pending.remove(&(record.container.clone(), record.key.clone())); }
        Ok(())
    }
    /// Persist the reconciled snapshot before switching *all* local bindings under
    /// the mutation gate. Failure leaves the live document and original intents intact.
    pub fn adopt_snapshot(&self, bytes: &[u8], expected_chat: Option<&str>) -> Result<(), DocError> {
        self.adopt_snapshot_scoped(bytes,expected_chat,self.scope_generation())
    }
    pub fn adopt_snapshot_scoped(&self, bytes: &[u8], expected_chat: Option<&str>, scope_generation: u64) -> Result<(), DocError> {
        let _operation = self.operation();
        if scope_generation != self.scope_generation() { return Err(DocError::Schema("retired room scope cannot replace the live binding".into())); }
        let candidate = LoroDoc::new();
        let status = candidate.import(bytes).map_err(|e| DocError::Schema(e.to_string()))?;
        if status.pending.as_ref().is_some_and(|v| !v.is_empty()) || candidate.is_detached() || candidate.state_vv() != candidate.oplog_vv() {
            return Err(DocError::Schema("replacement snapshot is incomplete".into()));
        }
        if let Some(chat) = expected_chat {
            if candidate.get_map("meta").get("chatId").map(|v| v.get_deep_value().to_json_value()) != Some(serde_json::Value::String(chat.into())) {
                return Err(DocError::Schema("replacement snapshot has a different public chat identity".into()));
            }
        }
        let mut pending = self.pending_records();
        reconcile(&candidate, &pending, &self.raw())?;
        let version = candidate.oplog_vv().encode();
        for record in &mut pending {
            record.version = version.clone();
            if matches!(record.container.as_str(), "commands" | "messages" | "agentSessions") {
                record.value = row_value(&candidate,&record.container,&record.key);
            }
        }
        let pending: Vec<_> = pending.into_iter().map(Arc::new).collect();
        let snapshot = candidate.export(ExportMode::Snapshot).map_err(|e| DocError::Schema(e.to_string()))?;
        if let Some(persist) = lock(&self.0.persist).as_ref() { persist(&pending, Some(&snapshot)).map_err(DocError::Schema)?; }
        let journal = journal_subscription(&self.0, &candidate);
        let baseline = map_rows(&candidate);
        let mut state = lock(&self.0.state);
        for observer in state.observers.values_mut() {
            observer.subscription = if let Some(callback) = &observer.root {
                candidate.subscribe_root(callback.clone())
            } else {
                let callback = observer.local.as_ref().expect("local observer").clone();
                candidate.subscribe_local_update(Box::new(move |bytes| { callback(bytes); true }))
            };
        }
        state.raw = candidate;
        state.journal = Some(journal);
        self.0.generation.fetch_add(1,std::sync::atomic::Ordering::Release);
        *lock(&self.0.baseline) = baseline;
        *lock(&self.0.error) = None;
        let callbacks: Vec<_> = state.observers.values().filter_map(|observer| observer.root.clone()).collect();
        drop(state);
        *lock(&self.0.pending) = pending.into_iter().map(|record| ((record.container.clone(),record.key.clone()),record)).collect();
        drop(_operation);
        for callback in callbacks { callback(DiffEvent { triggered_by: EventTriggerKind::Import, origin: "Crew snapshot adoption", current_target: None, events: Vec::new() }); }
        Ok(())
    }
}
impl From<LoroDoc> for SharedDocument { fn from(doc: LoroDoc) -> Self { Self::new(doc) } }
const MAPS: &[&str] = &["devices", "spaces", "chats", "sessions", "sessionRefs", "worktreeDeletions", "agentSessions"];
fn map_rows(raw: &LoroDoc) -> BTreeMap<(String,String), serde_json::Value> {
    let mut rows = BTreeMap::new();
    for name in MAPS {
        if let serde_json::Value::Object(values) = raw.get_map(*name).get_deep_value().to_json_value() {
            for (key, value) in values { rows.insert(((*name).into(), key), value); }
        }
    }
    if let Some(value) = raw.get_map("meta").get("directoryCompletedTurn") { rows.insert(("meta".into(), "directoryCompletedTurn".into()),value.get_deep_value().to_json_value()); }
    rows
}
fn root_name(id: &loro::ContainerID) -> Option<String> {
    match id { loro::ContainerID::Root { name, .. } => Some(name.to_string()), _ => None }
}
fn row_id(row: &loro::ValueOrContainer) -> Option<String> {
    let value = match row {
        loro::ValueOrContainer::Container(loro::Container::Map(map)) => map.get("id")?.get_deep_value(),
        loro::ValueOrContainer::Value(LoroValue::Map(fields)) => fields.get("id")?.clone(),
        _ => return None,
    };
    match value { LoroValue::String(id) => Some(id.to_string()), _ => None }
}

fn journal_subscription(inner: &Arc<Inner>, raw: &LoroDoc) -> loro::Subscription {
    let weak = Arc::downgrade(inner);
    // A subscription must not own its document: that forms a Loro callback cycle.
    raw.subscribe_root(Arc::new(move |event| {
        let Some(inner) = weak.upgrade() else { return };
        let source = lock(&inner.state).raw.clone();
        let mut changed = BTreeSet::new();
        for diff in &event.events {
            let Some(name) = root_name(diff.target).or_else(|| diff.path.first().and_then(|(id,_)| root_name(id))) else { continue };
            if name == "meta" {
                if let Diff::Map(map) = &diff.diff { if map.updated.contains_key("directoryCompletedTurn") { changed.insert((name, "directoryCompletedTurn".into())); } }
                continue;
            }
            if MAPS.contains(&name.as_str()) {
                // SDK paths include (root, Key(container)) before the row selector.
                if let Some((_, Index::Key(key))) = diff.path.get(1) { changed.insert((name, key.to_string())); }
                else if let Diff::Map(map) = &diff.diff { for key in map.updated.keys() { changed.insert((name.clone(), key.to_string())); } }
            } else if matches!(name.as_str(), "messages" | "commands" | "publications") {
                if let Some((_, Index::Seq(index))) = diff.path.get(1) {
                    if let Some(row) = source.get_list(name.as_str()).get(*index) {
                        if let Some(key) = row_id(&row) { changed.insert((name, key)); }
                    }
                } else if let Diff::List(items) = &diff.diff {
                    for item in items { if let ListDiffItem::Insert { insert, .. } = item { for row in insert {
                        if let Some(key) = row_id(row) { changed.insert((name.clone(), key)); }
                    } } }
                }
            }
        }
        let mut updates = Vec::new();
        for (container,key) in changed {
            let value = row_value(&source, &container, &key);
            let identity = (container.clone(),key.clone());
            let before = lock(&inner.baseline).get(&identity).cloned();
            let baseline_value = value.as_ref().filter(|_| MAPS.contains(&identity.0.as_str()) || identity.0 == "meta").cloned();
            if event.triggered_by == EventTriggerKind::Local {
                let mut pending = lock(&inner.pending);
                let record = pending.entry(identity.clone()).or_insert_with(|| Arc::new(PendingRecord { container, key, before, value: None, version: Vec::new(), acknowledged: false }));
                let current = Arc::make_mut(record);
                current.value = value;
                current.version = source.oplog_vv().encode();
                current.acknowledged = false;
                updates.push(record.clone());
            }
            let mut baseline = lock(&inner.baseline);
            if let Some(value) = baseline_value { baseline.insert(identity,value); } else { baseline.remove(&identity); }
        }
        if !updates.is_empty() { if let Some(persist) = lock(&inner.persist).as_ref() {
            if let Err(error) = persist(&updates,None) { *lock(&inner.error) = Some(format!("Crew could not persist accepted records: {error}")); }
        } }
    }))
}
fn row_value(doc: &LoroDoc, container: &str, key: &str) -> Option<serde_json::Value> {
    if container == "meta" { return doc.get_map(container).get(key).map(|v| v.get_deep_value().to_json_value()); }
    if MAPS.contains(&container) { return doc.get_map(container).get(key).map(|v| v.get_deep_value().to_json_value()); }
    let list = doc.get_list(container);
    for index in 0..list.len() { if let Some(row) = list.get(index) {
        let map = match &row { loro::ValueOrContainer::Container(loro::Container::Map(map)) => map, _ => continue };
        if matches!(map.get("id").map(|value| value.get_deep_value()), Some(LoroValue::String(id)) if id.as_str() == key) {
            return Some(row.get_deep_value().to_json_value());
        }
    } }
    None
}
fn reconcile(doc: &LoroDoc, pending: &[PendingRecord], cached: &LoroDoc) -> Result<(),DocError> {
    let session = SessionDoc::from_doc(doc.clone());
    let candidate_version = doc.oplog_vv();
    for record in pending.iter().filter(|record| record.container != "agentSessions" && record.container != "meta")
        .chain(pending.iter().filter(|record| record.container == "agentSessions" || record.container == "meta")) {
        let local_version = if record.version.is_empty() { None } else { loro::VersionVector::decode(&record.version).ok() };
        // A retained intent can already be superseded in the cached history.
        // An older causal checkpoint, conversely, cannot contain a competing edit.
        if local_version.as_ref().is_some_and(|version| candidate_version.includes_vv(version)) { continue }
        let remote = row_value(doc,&record.container,&record.key);
        if remote == record.value { continue }
        let conflict = || DocError::Schema(format!("Crew recovery conflict in {}/{}; original intent retained",record.container,record.key));
        match record.container.as_str() {
            "meta" if record.key == "directoryCompletedTurn" => {
                let Some(local) = record.value.as_ref().and_then(|value| value.as_str()) else { return Err(conflict()) };
                let local_time = session.read_entry(local)?.map(|entry| entry.created_at);
                let remote_time = remote.as_ref().and_then(|value| value.as_str()).map(|id| session.read_entry(id)).transpose()?.flatten().map(|entry| entry.created_at);
                if local_time.is_some() && local_time >= remote_time { session.set_completed_turn(local)?; }
            }
            "messages" => {
                let Some(value) = &record.value else { return Err(conflict()) };
                // Legacy migration uses app parts until adoption assigns its first version.
                let local: SessionMessageEntry = if record.version.is_empty() {
                    serde_json::from_value(value.clone())?
                } else { crate::schema::entry_from_json(value.clone())? };
                session.reconcile_message(&local)?;
            }
            "commands" => {
                let Some(value) = &record.value else { return Err(conflict()) };
                let mut local: SessionCommandEntry = serde_json::from_value(value.clone())?;
                let remote = remote.map(serde_json::from_value::<SessionCommandEntry>).transpose()?;
                if let Some(remote) = &remote {
                    if remote.payload != local.payload || remote.issued_by != local.issued_by || remote.issued_at != local.issued_at || remote.expires_at != local.expires_at || remote.based_on != local.based_on { return Err(conflict()) }
                    if remote.status != SessionCommandStatus::Pending {
                        if local.status != SessionCommandStatus::Pending && local.status != remote.status { return Err(conflict()) }
                        continue;
                    }
                }
                if local.status == SessionCommandStatus::Pending
                    && local.expires_at.is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp_millis())
                {
                    local.status = SessionCommandStatus::Expired;
                    local.resolution = Some("Command expired while offline".into());
                }
                if remote.is_some() {
                    if local.status != SessionCommandStatus::Pending { session.set_command_status(&local.id,local.status,local.resolution.as_deref())?; }
                } else { session.queue_command(&local)?; if local.resolution.is_some() { session.set_command_status(&local.id,local.status,local.resolution.as_deref())?; } }
            }
            "publications" => {
                if remote.is_some() { return Err(conflict()) }
                let record: PublicationRecord = serde_json::from_value(record.value.as_ref().and_then(|v| v.get("record")).cloned().ok_or_else(conflict)?)?;
                session.append_publication(&record)?;
            }
            "agentSessions" => {
                let Some(value) = &record.value else { return Err(conflict()) };
                let publication: PublicationRecord = serde_json::from_value(value.clone())?;
                session.upsert_agent_session(&publication)?;
            }
            name if MAPS.contains(&name) => {
                let local_follows_remote = local_version.as_ref().is_some_and(|version| {
                    if version.includes_vv(&candidate_version) { return true }
                    // ponytail: peer coverage is conservative; expose per-field op IDs if same-peer unrelated edits block recovery.
                    // Creation coalesces later accepted edits. Other-row writers must
                    // not make an already-observed creation look concurrent.
                    if record.before.is_some() { return false }
                    let (Some(local), Some(remote)) = (record.value.as_ref().and_then(serde_json::Value::as_object), remote.as_ref().and_then(serde_json::Value::as_object)) else { return false };
                    // Full-row replay cannot erase fields absent from the original creation intent.
                    if remote.keys().any(|field| !local.contains_key(field)) { return false }
                    let root = doc.get_map(name);
                    let covers_editor = |peer| candidate_version.get(&peer).is_some_and(|counter|
                        *counter > 0 && version.get(&peer).is_some_and(|accepted| *accepted >= *counter));
                    if !root.get_last_editor(&record.key).is_some_and(covers_editor) { return false }
                    let Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) = root.get(&record.key) else { return false };
                    local.keys().all(|field| match row.get_last_editor(field) {
                        Some(peer) => covers_editor(peer),
                        None => !remote.contains_key(field),
                    })
                });
                let cached_session = if name == "sessions" { row_value(cached, name, &record.key) } else { None };
                let owner_publication = if name == "sessions" && remote.is_some() && record.value.is_some() {
                    let local = record.value.as_ref().and_then(serde_json::Value::as_object).ok_or_else(conflict)?;
                    let remote = remote.as_ref().and_then(serde_json::Value::as_object).ok_or_else(conflict)?;
                    let before = match &record.before { Some(value) => Some(value.as_object().ok_or_else(conflict)?), None => None };
                    let cached = match &cached_session { Some(value) => Some(value.as_object().ok_or_else(conflict)?), None => None };
                    let owner = local.get("deviceId").and_then(serde_json::Value::as_str).filter(|owner| !owner.is_empty()).ok_or_else(conflict)?;
                    for row in before.into_iter().chain([local, remote]).chain(cached) {
                        if row.get("chatId").and_then(serde_json::Value::as_str) != Some(record.key.as_str())
                            || row.get("deviceId").and_then(serde_json::Value::as_str) != Some(owner)
                            || !matches!(row.get("status").and_then(serde_json::Value::as_str), Some("idle" | "working" | "awaitingInput" | "errored"))
                            || row.get("updatedAt").and_then(serde_json::Value::as_i64).and_then(chrono::DateTime::from_timestamp_millis).is_none()
                            || row.get("startedAt").is_some_and(|value| value.as_i64().and_then(chrono::DateTime::from_timestamp_millis).is_none())
                        { return Err(conflict()) }
                    }
                    let local_at = local["updatedAt"].as_i64().expect("validated owner clock");
                    let remote_at = remote["updatedAt"].as_i64().expect("validated owner clock");
                    // An unchanged incoming publication is the three-way baseline,
                    // even when millisecond encoding ties a genuine local transition.
                    let local_publication_wins_tie = local_follows_remote || before.is_some_and(|row|
                        ["status", "startedAt", "updatedAt"].iter().all(|field| row.get(*field) == remote.get(*field)));
                    if before.is_some_and(|row| row["updatedAt"].as_i64().is_some_and(|at| local_at < at || remote_at < at))
                        || (!local_publication_wins_tie && local_at == remote_at && ["status", "startedAt"].iter().any(|field| local.get(*field) != remote.get(*field)))
                    { return Err(conflict()) }
                    // Status and run identity belong to the clocked publication, never to separate field winners.
                    let mut publication = if local_at > remote_at || (local_at == remote_at && local_publication_wins_tie) { local } else { remote };
                    if let Some(cached) = cached {
                        let cached_at = cached["updatedAt"].as_i64().expect("validated owner clock");
                        let at = publication["updatedAt"].as_i64().expect("validated owner clock");
                        if cached_at == at && ["status", "startedAt"].iter().any(|field| cached.get(*field) != publication.get(*field)) { return Err(conflict()) }
                        if cached_at > at { publication = cached; }
                    }
                    Some(publication)
                } else { None };
                let mut merged = if local_follows_remote {
                    record.value.clone()
                } else { match (&record.before,&record.value,&remote) {
                    (_,None,None) => continue,
                    (Some(before),None,Some(remote)) if before == remote => None,
                    (None,Some(local),None) => {
                        if doc.get_map(name).get_last_editor(&record.key).is_some() { return Err(conflict()) }
                        Some(local.clone())
                    }
                    (None,Some(local),Some(remote)) if name == "devices"
                        && local.get("id").and_then(|value| value.as_str()) == Some(record.key.as_str())
                        && remote.get("id") == local.get("id") => {
                        let Some(mut merged) = remote.as_object().cloned() else { return Err(conflict()) };
                        // Boot owns runtime identity/liveness, not a remote user's device name.
                        for field in ["platform", "environment", "namespaceDevboxId", "version", "lastSeenAt"] {
                            if let Some(value) = local.get(field) {
                                if field != "lastSeenAt" || value.as_i64() > remote.get(field).and_then(|value| value.as_i64()) { merged.insert(field.into(),value.clone()); }
                            }
                        }
                        Some(serde_json::Value::Object(merged))
                    }
                    (before,Some(local),Some(remote)) if before.is_some() || owner_publication.is_some() => {
                        let before = match before { Some(value) => Some(value.as_object().ok_or_else(conflict)?), None => None };
                        let (Some(local),Some(remote)) = (local.as_object(),remote.as_object()) else { return Err(conflict()) };
                        let mut merged = remote.clone();
                        for field in before.into_iter().flat_map(|row| row.keys()).chain(local.keys()).collect::<BTreeSet<_>>() {
                            if owner_publication.is_some() && matches!(field.as_str(), "status" | "startedAt" | "updatedAt") { continue }
                            if before.and_then(|row| row.get(field)) == local.get(field) { continue }
                            if name == "chats" && field == "lastMessageAt"
                                && local.get("id").and_then(serde_json::Value::as_str) == Some(record.key.as_str())
                                && remote.get("id") == local.get("id") && before.and_then(|row| row.get("deviceId")) == local.get("deviceId")
                                && local.get("deviceId").and_then(serde_json::Value::as_str).is_some_and(|owner| remote.get("deviceId").and_then(serde_json::Value::as_str) == Some(owner))
                                && let (Some(local_at), Some(remote_at)) = (local.get(field).and_then(serde_json::Value::as_i64), remote.get(field).and_then(serde_json::Value::as_i64))
                                && before.and_then(|row| row.get(field)).and_then(serde_json::Value::as_i64).is_none_or(|at| local_at >= at && remote_at >= at)
                            {
                                // Activity is a monotonic owner clock, not conflicting user intent.
                                merged.insert(field.clone(), serde_json::json!(local_at.max(remote_at)));
                                continue;
                            }
                            if remote.get(field) != before.and_then(|row| row.get(field)) && remote.get(field) != local.get(field) {
                                return Err(DocError::Schema(format!("Crew recovery conflict in {}/{} field={field}; original intent retained", record.container, record.key)));
                            }
                            if let Some(value) = local.get(field) { merged.insert(field.clone(),value.clone()); } else { merged.remove(field); }
                        }
                        Some(serde_json::Value::Object(merged))
                    }
                    (_,Some(local),Some(remote)) if local == remote => continue,
                    _ => return Err(conflict()),
                } };
                if let (Some(publication), Some(serde_json::Value::Object(fields))) = (owner_publication, &mut merged) {
                    for field in ["status", "startedAt", "updatedAt"] {
                        if let Some(value) = publication.get(field) { fields.insert(field.into(), value.clone()); } else { fields.remove(field); }
                    }
                }
                let root = doc.get_map(name);
                if let Some(serde_json::Value::Object(fields)) = merged {
                    let row = match root.get(&record.key) { Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) => row, _ => root.insert_container(&record.key,LoroMap::new())? };
                    let old = row.get_deep_value().to_json_value();
                    if let Some(old) = old.as_object() { for field in old.keys() { if !fields.contains_key(field) { row.delete(field)?; } } }
                    for (field,value) in fields { if old.get(&field) != Some(&value) { row.insert(&field,LoroValue::from(value))?; } }
                } else { root.delete(&record.key)?; }
            }
            _ => return Err(conflict()),
        }
    }
    doc.commit();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MessagePart, MessageRole, MessageStatus, SessionCommandPayload, WorkspaceDoc};

    fn journal(binding: &SharedDocument) { binding.install_journal(Vec::new(), Arc::new(|_,_| Ok(()))); }
    fn message(id: &str) -> SessionMessageEntry {
        SessionMessageEntry { id: id.into(), role: MessageRole::User,
            parts: vec![MessagePart::Text { id: "text".into(), text: "accepted offline".into() }],
            created_at: 100, device_id: "device".into(), status: Some(MessageStatus::Complete),
            continuation_of: None, peer_message: None }
    }

    #[test]
    fn reused_snapshot_buffer_preserves_full_history_and_checksum() {
        let binding = SharedDocument::new(LoroDoc::new());
        let raw = binding.raw();
        let text = raw.get_text("command");
        let chunk = "α\\\"\nβ".repeat(1024);
        let mut bytes = Vec::with_capacity(8);
        bytes.extend_from_slice(b"stale");
        let mut storage = None;
        for turn in 1..=3 {
            text.insert(text.len_unicode(), &chunk).unwrap();
            binding.commit().unwrap();
            raw.inner().export_snapshot_into(&mut bytes).unwrap();
            if let Some(storage) = storage {
                assert_eq!(bytes.as_ptr(), storage, "sufficient caller capacity must be reused");
            }
            assert_eq!(bytes, binding.export(ExportMode::Snapshot).unwrap());
            let restored = LoroDoc::new();
            restored.import(&bytes).unwrap();
            assert_eq!(restored.get_text("command").to_string(), chunk.repeat(turn));
            assert_eq!(restored.oplog_vv(), raw.oplog_vv());
            if storage.is_none() {
                bytes.reserve(256 * 1024);
                storage = Some(bytes.as_ptr());
            }
        }
        *bytes.last_mut().unwrap() ^= 1;
        assert!(LoroDoc::new().import(&bytes).is_err(), "reused exports must retain checksum validation");
    }

    #[test]
    fn snapshot_between_text_appends_retains_only_original_arena_text() {
        let binding = SharedDocument::new(LoroDoc::new());
        let raw = binding.raw();
        let text = raw.get_text("command");
        let chunk = "x".repeat(4 * 1024);
        for turn in 1..=3 {
            text.insert(text.len_unicode(), &chunk).unwrap();
            binding.commit().unwrap();
            let snapshot = binding.export(ExportMode::Snapshot).unwrap();
            assert_eq!(raw.with_oplog(|oplog| oplog.arena_utf16_len()), turn * 4 * 1024,
                "snapshot export must not cause old text to be decoded into the live arena again");
            let restored = LoroDoc::new();
            restored.import(&snapshot).unwrap();
            assert_eq!(restored.get_text("command").to_string(), chunk.repeat(turn));
            assert_eq!(restored.oplog_vv(), raw.oplog_vv());
        }
        assert_eq!(text.len_utf16(), 12 * 1024);
    }

    #[test]
    fn compacted_workspace_replays_creation_and_field_rename_without_duplicates() {
        let remote = WorkspaceDoc::new();
        let row = remote.doc().get_map("chats").insert_container("chat",LoroMap::new()).unwrap();
        row.insert("id","chat").unwrap(); row.insert("deviceId","device").unwrap(); row.insert("title","before").unwrap();
        remote.doc().commit();
        let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
        let local = WorkspaceDoc::from_doc(raw);
        let binding = local.binding(); journal(&binding);
        local.rename_chat("chat","renamed offline").unwrap();
        let row = binding.get_map("chats").insert_container("offline",LoroMap::new()).unwrap();
        row.insert("id","offline").unwrap(); row.insert("deviceId","device").unwrap(); row.insert("title","created offline").unwrap();
        binding.commit().unwrap();
        let snapshot = remote.doc().export(ExportMode::shallow_snapshot(&remote.doc().state_frontiers())).unwrap();
        binding.adopt_snapshot(&snapshot,None).unwrap();
        binding.adopt_snapshot(&snapshot,None).unwrap();
        let chats = binding.get_map("chats").get_deep_value().to_json_value();
        assert_eq!(chats["chat"]["title"],"renamed offline");
        assert_eq!(chats["offline"]["title"],"created offline");
        assert_eq!(chats.as_object().unwrap().len(),2);
    }

    #[test]
    fn unrelated_remote_edits_preserve_causally_accepted_chat_creation() {
        let local = WorkspaceDoc::new();
        let binding = local.binding(); journal(&binding);
        let row = binding.get_map("chats").insert_container("chat", LoroMap::new()).unwrap();
        row.insert("id", "chat").unwrap(); row.insert("deviceId", "owner-device").unwrap();
        row.insert("title", "created").unwrap(); binding.commit().unwrap();
        let raw = LoroDoc::new(); raw.import(&local.export_snapshot().unwrap()).unwrap();
        let remote = WorkspaceDoc::from_doc(raw);
        local.rename_chat("chat", "accepted offline rename").unwrap();
        let original = binding.pending_records()[0].clone();
        let device = remote.doc().get_map("devices").insert_container("peer", LoroMap::new()).unwrap();
        device.insert("id", "peer").unwrap(); device.insert("name", "unrelated heartbeat").unwrap();
        remote.doc().commit();
        let snapshot = remote.export_snapshot().unwrap();
        binding.adopt_snapshot(&snapshot, None).unwrap();
        assert_eq!(local.chat("chat").unwrap().unwrap().title.as_deref(), Some("accepted offline rename"));
        assert_eq!(binding.get_map("devices").get("peer").unwrap().get_deep_value().to_json_value()["name"], "unrelated heartbeat");
        assert_eq!(binding.pending_records()[0].before, original.before);
        assert_eq!(binding.pending_records()[0].value, original.value);
        let preserved = local.export_snapshot().unwrap();
        let intents = serde_json::to_vec(&binding.pending_records()).unwrap();
        for (field, value) in [("title", "conflicting rename"), ("deviceId", "foreign-owner")] {
            let raw = LoroDoc::new(); raw.import(&snapshot).unwrap();
            let changed = WorkspaceDoc::from_doc(raw);
            let loro::ValueOrContainer::Container(loro::Container::Map(row)) = changed.doc().get_map("chats").get("chat").unwrap() else { panic!("chat row missing"); };
            row.insert(field, value).unwrap(); changed.doc().commit();
            assert!(binding.adopt_snapshot(&changed.export_snapshot().unwrap(), None).is_err());
            assert_eq!(local.export_snapshot().unwrap(), preserved);
            assert_eq!(serde_json::to_vec(&binding.pending_records()).unwrap(), intents);
        }
    }

    #[test]
    fn completion_activity_recovery_keeps_latest_owner_clock_and_concurrent_title() {
        for (local_at, remote_at) in [(3_000, 2_000), (2_000, 3_000)] {
            let remote = WorkspaceDoc::new();
            let row = remote.doc().get_map("chats").insert_container("chat", LoroMap::new()).unwrap();
            row.insert("id", "chat").unwrap(); row.insert("deviceId", "owner-device").unwrap();
            row.insert("title", "before").unwrap(); row.insert("lastMessageAt", 1_000i64).unwrap();
            remote.doc().commit();
            let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
            let local = WorkspaceDoc::from_doc(raw);
            let binding = local.binding(); journal(&binding);
            local.set_chat_activity("chat", Some(local_at), None).unwrap();
            remote.set_chat_activity("chat", Some(remote_at), None).unwrap();
            remote.rename_chat("chat", "concurrent title").unwrap();
            let snapshot = remote.doc().export(ExportMode::shallow_snapshot(&remote.doc().state_frontiers())).unwrap();
            binding.adopt_snapshot(&snapshot, None).unwrap();
            binding.adopt_snapshot(&snapshot, None).unwrap();
            let row = binding.get_map("chats").get("chat").unwrap().get_deep_value().to_json_value();
            assert_eq!(row["lastMessageAt"], 3_000);
            assert_eq!(row["title"], "concurrent title");
            assert_eq!(row["deviceId"], "owner-device");
            let foreign = remote.doc().get_map("chats").get("chat").unwrap();
            let loro::ValueOrContainer::Container(loro::Container::Map(foreign)) = foreign else { panic!("chat row missing"); };
            foreign.insert("deviceId", "foreign-owner").unwrap();
            foreign.insert("lastMessageAt", 4_000i64).unwrap(); remote.doc().commit();
            let changed_owner = remote.doc().export(ExportMode::shallow_snapshot(&remote.doc().state_frontiers())).unwrap();
            assert!(binding.adopt_snapshot(&changed_owner, None).is_err());
            assert_eq!(binding.get_map("chats").get("chat").unwrap().get_deep_value().to_json_value(), row);
        }
    }

    #[test]
    fn restarted_session_publications_keep_latest_owner_status_and_retain_conflicting_originals() {
        use comet_proto::{Session, SessionStatus};
        let publication = |at, status| Session {
            chat_id: "chat".into(), device_id: "owner-device".into(), status,
            started_at: chrono::DateTime::from_timestamp_millis(at - 100),
            updated_at: chrono::DateTime::from_timestamp_millis(at).unwrap(),
        };
        for existing in [false, true] {
            for (local_at, remote_at) in [(3_000, 2_000), (2_000, 3_000)] {
                let remote = WorkspaceDoc::new();
                if existing { remote.upsert_session(&publication(1_000, SessionStatus::Working)).unwrap(); }
                let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
                let local = WorkspaceDoc::from_doc(raw);
                let original = local.binding(); journal(&original);
                let local_session = publication(local_at, SessionStatus::Idle);
                let remote_session = publication(remote_at, SessionStatus::Working);
                local.upsert_session(&local_session).unwrap();
                let loro::ValueOrContainer::Container(loro::Container::Map(row)) = original.get_map("sessions").get("chat").unwrap() else { panic!("session row missing"); };
                row.insert("label", "offline intent").unwrap(); original.commit().unwrap();
                let records = serde_json::to_vec(&original.pending_records()).unwrap();
                let raw = LoroDoc::new(); raw.import(&local.export_snapshot().unwrap()).unwrap();
                let restarted = WorkspaceDoc::from_doc(raw);
                let binding = restarted.binding();
                binding.install_journal(serde_json::from_slice(&records).unwrap(), Arc::new(|_,_| Ok(())));
                remote.upsert_session(&remote_session).unwrap();
                let loro::ValueOrContainer::Container(loro::Container::Map(row)) = remote.doc().get_map("sessions").get("chat").unwrap() else { panic!("session row missing"); };
                row.insert("remoteNote", "concurrent field").unwrap(); remote.doc().commit();
                let snapshot = remote.doc().export(ExportMode::shallow_snapshot(&remote.doc().state_frontiers())).unwrap();
                for _ in 0..2 {
                    binding.adopt_snapshot(&snapshot, None).unwrap();
                    assert_eq!(restarted.read_sessions().unwrap(), vec![if local_at > remote_at { local_session.clone() } else { remote_session.clone() }]);
                    let row = binding.get_map("sessions").get("chat").unwrap().get_deep_value().to_json_value();
                    assert_eq!(row["label"], "offline intent");
                    assert_eq!(row["remoteNote"], "concurrent field");
                    let pending: Vec<PendingRecord> = serde_json::from_slice(&records).unwrap();
                    assert_eq!(binding.pending_records()[0].before, pending[0].before);
                    assert_eq!(binding.pending_records()[0].value, pending[0].value);
                }
                // An older owner snapshot must not resurrect a losing offline status after recovery.
                let stale = LoroDoc::new(); stale.import(&snapshot).unwrap();
                let loro::ValueOrContainer::Container(loro::Container::Map(row)) = stale.get_map("sessions").get("chat").unwrap() else { panic!("session row missing"); };
                row.insert("updatedAt", 1_500i64).unwrap(); row.insert("status", "errored").unwrap(); stale.commit();
                binding.adopt_snapshot(&stale.export(ExportMode::shallow_snapshot(&stale.state_frontiers())).unwrap(), None).unwrap();
                assert_eq!(restarted.read_sessions().unwrap(), vec![if local_at > remote_at { local_session.clone() } else { remote_session.clone() }]);
                let original_cache = restarted.export_snapshot().unwrap();
                let original_records = serde_json::to_vec(&binding.pending_records()).unwrap();
                for (field, value) in [
                    ("deviceId", serde_json::json!("foreign-owner")),
                    ("chatId", serde_json::json!("foreign-chat")),
                    ("updatedAt", serde_json::json!("corrupt-clock")),
                    ("updatedAt", serde_json::json!(999)),
                    ("updatedAt", serde_json::json!(local_at)),
                    ("status", serde_json::json!("corrupt-status")),
                    ("label", serde_json::json!("divergent intent")),
                ] {
                    if field == "updatedAt" && value == serde_json::json!(999) && !existing { continue }
                    // Start every boundary case from the same accepted owner publication.
                    let candidate = LoroDoc::new(); candidate.import(&snapshot).unwrap();
                    let loro::ValueOrContainer::Container(loro::Container::Map(row)) = candidate.get_map("sessions").get("chat").unwrap() else { panic!("session row missing"); };
                    row.insert(field, LoroValue::from(value)).unwrap(); candidate.commit();
                    assert!(binding.adopt_snapshot(&candidate.export(ExportMode::shallow_snapshot(&candidate.state_frontiers())).unwrap(), None).is_err(), "must retain {field} conflict");
                    assert_eq!(restarted.export_snapshot().unwrap(), original_cache);
                    assert_eq!(serde_json::to_vec(&binding.pending_records()).unwrap(), original_records);
                }
            }
        }
        // Millisecond encoding may tie a causal status transition; its ancestry still orders it.
        let remote = WorkspaceDoc::new();
        remote.upsert_session(&publication(1_000, SessionStatus::Working)).unwrap();
        let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
        let local = WorkspaceDoc::from_doc(raw); let binding = local.binding(); journal(&binding);
        let settled = publication(1_000, SessionStatus::Idle);
        local.upsert_session(&settled).unwrap();
        binding.adopt_snapshot(&remote.export_snapshot().unwrap(), None).unwrap();
        assert_eq!(local.read_sessions().unwrap(), vec![settled]);
    }

    #[test]
    fn conflicting_field_and_foreign_room_leave_original_cache_and_intent_untouched() {
        let doc = SessionDoc::init("public-chat").unwrap();
        let binding = doc.binding(); journal(&binding);
        doc.push_message(&message("message")).unwrap();
        let version = binding.oplog_vv();
        let foreign = SessionDoc::init("another-chat").unwrap();
        assert!(binding.adopt_snapshot(&foreign.export_snapshot().unwrap(),Some("public-chat")).is_err());
        assert_eq!(binding.oplog_vv(),version);
        assert_eq!(doc.read_entries().unwrap(),vec![message("message")]);

        let remote = WorkspaceDoc::new();
        let row = remote.doc().get_map("chats").insert_container("chat",LoroMap::new()).unwrap();
        row.insert("id","chat").unwrap(); row.insert("deviceId","device").unwrap(); row.insert("title","base").unwrap(); remote.doc().commit();
        let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
        let local = WorkspaceDoc::from_doc(raw); let binding = local.binding(); journal(&binding);
        local.rename_chat("chat","local intent").unwrap();
        remote.rename_chat("chat","remote intent").unwrap();
        assert!(binding.adopt_snapshot(&remote.export_snapshot().unwrap(),None).is_err());
        assert_eq!(binding.get_map("chats").get("chat").unwrap().get_deep_value().to_json_value()["title"],"local intent");
        assert_eq!(binding.pending_records()[0].value.as_ref().unwrap()["title"],"local intent");
    }

    #[test]
    fn command_terminal_outcomes_and_expiry_survive_replacement_and_acknowledgement() {
        let remote = SessionDoc::init("chat").unwrap();
        let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
        let local = SessionDoc::from_doc(raw); let binding = local.binding(); journal(&binding);
        let entry = SessionCommandEntry { id:"command".into(), payload:SessionCommandPayload::Interrupt {}, issued_by:"device".into(),
            issued_at:0,based_on:None,expires_at:Some(1),status:SessionCommandStatus::Pending,resolution:None };
        local.queue_command(&entry).unwrap();
        let mut applied = entry.clone(); applied.id="applied".into(); applied.status=SessionCommandStatus::Applied;
        local.queue_command(&applied).unwrap(); local.set_command_status("applied",SessionCommandStatus::Applied,Some("durable outcome")).unwrap();
        let snapshot = remote.doc().export(ExportMode::shallow_snapshot(&remote.doc().state_frontiers())).unwrap();
        binding.adopt_snapshot(&snapshot,Some("chat")).unwrap();
        assert_eq!(local.read_command("command").unwrap().unwrap().status,SessionCommandStatus::Expired);
        assert_eq!(local.read_command("applied").unwrap().unwrap().resolution.as_deref(),Some("durable outcome"));
        binding.adopt_snapshot(&snapshot,Some("chat")).unwrap();
        assert_eq!(local.read_commands().unwrap().len(),2);
        binding.acknowledge(&binding.oplog_vv()).unwrap();
        assert!(binding.pending_records().is_empty());
        assert_eq!(local.read_command("applied").unwrap().unwrap().status,SessionCommandStatus::Applied);
    }

    #[test]
    fn replacement_preserves_document_encoded_questions_and_resolved_tools() {
        let remote = SessionDoc::init("chat").unwrap();
        let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
        let local = SessionDoc::from_doc(raw); let binding = local.binding(); journal(&binding);
        let entry = SessionMessageEntry { id:"question-and-tool".into(), role:MessageRole::Assistant,
            parts:vec![
                MessagePart::Input { id:"request".into(), request_id:"request".into(), resolved:false,
                    questions:vec![comet_proto::UserInputQuestion { id:"choice".into(), header:"Proceed".into(),
                        question:"Apply the change?".into(), options:vec!["Yes".into(),"No".into()], multi_select:false }] },
                MessagePart::Tool { id:"tool".into(), call:comet_proto::ToolCall::Exec { command:"echo complete".into() },
                    is_error:false, resolved:true },
            ], created_at:100, device_id:"device".into(), status:Some(MessageStatus::Streaming),
            continuation_of:None, peer_message:None };
        local.push_message(&entry).unwrap();
        let snapshot = remote.doc().export(ExportMode::shallow_snapshot(&remote.doc().state_frontiers())).unwrap();
        binding.adopt_snapshot(&snapshot,Some("chat")).unwrap();
        assert_eq!(local.read_entries().unwrap(),vec![entry]);
    }

    #[test]
    fn remote_settlement_wins_over_offline_expiry_after_lost_acknowledgement() {
        for status in [SessionCommandStatus::Applied,SessionCommandStatus::Rejected] {
            let remote = SessionDoc::init("chat").unwrap();
            let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
            let local = SessionDoc::from_doc(raw); let binding = local.binding(); journal(&binding);
            let command = SessionCommandEntry { id:"settled".into(), payload:SessionCommandPayload::Interrupt {},
                issued_by:"device".into(), issued_at:0, based_on:None, expires_at:Some(1),
                status:SessionCommandStatus::Pending, resolution:None };
            local.queue_command(&command).unwrap();
            let mut outcome = command.clone(); outcome.status = status; outcome.resolution = Some("owner settled".into());
            remote.queue_command(&outcome).unwrap();
            remote.set_command_status(&outcome.id,status,outcome.resolution.as_deref()).unwrap();
            let snapshot = remote.doc().export(ExportMode::shallow_snapshot(&remote.doc().state_frontiers())).unwrap();
            binding.adopt_snapshot(&snapshot,Some("chat")).unwrap();
            assert_eq!(local.read_commands().unwrap(),vec![outcome]);
        }
    }

    #[test]
    fn replacement_remounts_local_observers_without_retaining_retired_documents() {
        let doc = SessionDoc::init("chat").unwrap(); let binding = doc.binding(); journal(&binding);
        let local_events = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = local_events.clone(); let subscription = binding.subscribe_local_update(Arc::new(move |_| { counter.fetch_add(1,std::sync::atomic::Ordering::Relaxed); }));
        doc.push_message(&message("offline")).unwrap();
        let before = local_events.load(std::sync::atomic::Ordering::Relaxed);
        let remote = SessionDoc::init("chat").unwrap();
        binding.adopt_snapshot(&remote.export_snapshot().unwrap(),Some("chat")).unwrap();
        doc.push_message(&message("after")).unwrap();
        assert_eq!(local_events.load(std::sync::atomic::Ordering::Relaxed),before+1);
        assert_eq!(doc.read_entries().unwrap().iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(),vec!["offline","after"]);
        drop(subscription);
        assert!(lock(&binding.0.state).observers.is_empty());
        let weak = Arc::downgrade(&binding.0); drop(binding); drop(doc);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn adoption_callbacks_can_accept_a_new_local_message_without_deadlock() {
        let doc = Arc::new(SessionDoc::init("chat").unwrap()); let binding = doc.binding(); journal(&binding);
        let weak = Arc::downgrade(&doc);
        let _subscription = binding.subscribe_root(Arc::new(move |event| {
            if event.origin == "Crew snapshot adoption" { weak.upgrade().unwrap().push_message(&message("callback")).unwrap(); }
        }));
        let remote = SessionDoc::init("chat").unwrap();
        binding.adopt_snapshot(&remote.export_snapshot().unwrap(),Some("chat")).unwrap();
        assert_eq!(doc.read_entries().unwrap(),vec![message("callback")]);
        assert_eq!(binding.pending_records()[0].key,"callback");
    }

    #[test]
    fn concurrent_accepted_appends_survive_adoption_and_imports_never_echo_as_local_intent() {
        let remote = SessionDoc::init("chat").unwrap();
        let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
        let doc = Arc::new(SessionDoc::from_doc(raw)); let binding = doc.binding(); journal(&binding);
        remote.push_message(&message("remote")).unwrap();
        binding.import(&remote.export_snapshot().unwrap()).unwrap();
        assert!(binding.pending_records().is_empty());
        let snapshot = remote.doc().export(ExportMode::shallow_snapshot(&remote.doc().state_frontiers())).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let writer_doc = doc.clone(); let writer_barrier = barrier.clone();
        let writer = std::thread::spawn(move || {
            writer_barrier.wait();
            for index in 0..100 { writer_doc.push_message(&message(&format!("local-{index}"))).unwrap(); }
        });
        barrier.wait(); binding.adopt_snapshot(&snapshot,Some("chat")).unwrap(); writer.join().unwrap();
        let entries = doc.read_entries().unwrap();
        let ids: BTreeSet<_> = entries.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(entries.len(),101); assert_eq!(ids.len(),101);
        for index in 0..100 { assert!(ids.contains(format!("local-{index}").as_str())); }
        assert!(ids.contains("remote"));
    }
}
