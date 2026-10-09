//! Replaceable Loro cache binding. Durable semantic intents do not depend on Loro ancestry.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use loro::{ContainerTrait, EventTriggerKind, ExportMode, Index, LoroDoc, LoroList, LoroMap, LoroValue, ToJson};
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
        // Alias normalization creates local operations, not unseen remote edits.
        // Reconciliation must use the causal frontiers of the imported histories.
        let candidate_version = candidate.oplog_vv();
        let original_cache = self.raw();
        let cached_version = original_cache.oplog_vv();
        let mut pending = self.pending_records();
        let mut retired = Vec::new();
        let cached = if expected_chat.is_none() {
            crate::WorkspaceDoc::from_doc(candidate.clone()).migrate_execution_rows()?;
            let cached = original_cache.fork();
            crate::WorkspaceDoc::from_doc(cached.clone()).migrate_execution_rows()?;
            let originals = std::mem::take(&mut pending);
            for record in &originals {
                let normalized = crate::workspace::normalize_workspace_record(record)?;
                if normalized.key != record.key {
                    let mut original = record.clone(); original.acknowledged = true;
                    retired.push(Arc::new(original));
                    if let Some(canonical) = originals.iter().find(|canonical| canonical.container == record.container && canonical.key == normalized.key) {
                        let target = crate::workspace::normalize_workspace_record(canonical)?;
                        if (target.before == normalized.before && target.value == normalized.value)
                            || crate::workspace::canonical_creation_covers_alias(record, canonical, &original_cache) { continue }
                    }
                }
                pending.push(normalized);
            }
            let mut keys = BTreeSet::new();
            if pending.iter().any(|record| !keys.insert((record.container.clone(), record.key.clone()))) {
                return Err(DocError::Schema("Crew recovery has competing canonical workspace intents; originals retained".into()));
            }
            cached
        } else { original_cache };
        // An authenticated older checkpoint must not roll back accepted local history.
        // The normalized workspace cache is an isolated fork; persistence stays atomic.
        // Retained deletions still conflict with unobserved remote resurrections.
        let preserve_cached = expected_chat.is_none()
            && !cached.is_detached() && cached.state_vv() == cached.oplog_vv()
            && cached_version.includes_vv(&candidate_version)
            && pending.iter().all(|record| row_value(&cached, &record.container, &record.key) == record.value);
        let candidate = if preserve_cached { cached.clone() } else { candidate };
        reconcile(&candidate, &pending, &cached, expected_chat.is_some(),
            if preserve_cached { &cached_version } else { &candidate_version }, &cached_version)?;
        let version = candidate.oplog_vv().encode();
        for record in &mut pending {
            record.version = version.clone();
            if matches!(record.container.as_str(), "commands" | "messages" | "agentSessions") {
                record.value = row_value(&candidate,&record.container,&record.key);
            }
        }
        let pending: Vec<_> = pending.into_iter().map(Arc::new).collect();
        let snapshot = candidate.export(ExportMode::Snapshot).map_err(|e| DocError::Schema(e.to_string()))?;
        if let Some(persist) = lock(&self.0.persist).as_ref() {
            retired.extend(pending.iter().cloned());
            persist(&retired, Some(&snapshot)).map_err(DocError::Schema)?;
        }
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
fn reconcile(
    doc: &LoroDoc, pending: &[PendingRecord], cached: &LoroDoc, session_context: bool,
    candidate_version: &loro::VersionVector, cached_version: &loro::VersionVector,
) -> Result<(),DocError> {
    let session = std::cell::LazyCell::new(|| SessionDoc::from_doc(doc.clone()));
    if session_context { std::cell::LazyCell::force(&session); }
    for record in pending.iter().filter(|record| record.container != "agentSessions" && record.container != "meta")
        .chain(pending.iter().filter(|record| record.container == "agentSessions" || record.container == "meta")) {
        let local_version = if record.version.is_empty() { None } else { loro::VersionVector::decode(&record.version).ok() };
        // A retained intent can already be superseded in the cached history.
        // An older causal checkpoint, conversely, cannot contain a competing edit.
        let covered = local_version.as_ref().is_some_and(|version| candidate_version.includes_vv(version));
        if covered && !MAPS.contains(&record.container.as_str()) { continue }
        let remote = row_value(doc,&record.container,&record.key);
        let conflict = || DocError::Schema(format!("Crew recovery conflict in {}/{}; original intent retained",record.container,record.key));
        // Owner-register publication IDs change; they are not logical map row identities.
        if MAPS.contains(&record.container.as_str()) && record.container != "agentSessions" {
            if let Some(base) = record.before.as_ref().or(record.value.as_ref()).and_then(serde_json::Value::as_object) {
                for row in record.value.as_ref().and_then(serde_json::Value::as_object).into_iter()
                    .chain(remote.as_ref().and_then(serde_json::Value::as_object)) {
                    if crate::workspace::WORKSPACE_IDENTITY_FIELDS.iter().any(|field| row.get(*field) != base.get(*field)) { return Err(conflict()) }
                }
                if !crate::workspace::workspace_routes_compatible(&record.container,
                    record.before.as_ref().and_then(serde_json::Value::as_object).into_iter()
                        .chain(remote.as_ref().and_then(serde_json::Value::as_object))
                        .chain(record.value.as_ref().and_then(serde_json::Value::as_object))) { return Err(conflict()) }
            }
        }
        if covered || remote == record.value { continue }
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
                let seed_merge = if matches!(name, "chats" | "sessionRefs") && record.before.is_none() {
                    record.value.as_ref().and_then(serde_json::Value::as_object).and_then(|local| {
                        let remote = remote.as_ref().and_then(serde_json::Value::as_object)?;
                        if name == "chats" {
                            if local.get("id").and_then(serde_json::Value::as_str) != Some(record.key.as_str())
                                || !local.get("deviceId").and_then(serde_json::Value::as_str).is_some_and(|owner| !owner.is_empty() && remote.get("deviceId").and_then(serde_json::Value::as_str) == Some(owner)) { return None }
                        } else {
                            let user = local.get("userId").and_then(serde_json::Value::as_str).filter(|user| !user.is_empty())?;
                            let chat = local.get("chatId").and_then(serde_json::Value::as_str).filter(|chat| !chat.is_empty())?;
                            let (length, key) = record.key.split_once(':')?;
                            if length.parse::<usize>().ok() != Some(user.len()) || key.strip_prefix(user)?.strip_prefix(':')? != chat { return None }
                        }
                        let clock = |row: &serde_json::Map<String, serde_json::Value>, field: &str| row.get(field).and_then(serde_json::Value::as_i64)
                            .filter(|at| chrono::DateTime::from_timestamp_millis(*at).is_some());
                        let activity_fields: &[&str] = if name == "chats" { &["lastMessageAt", "lastMessagePreview"] } else { &[] };
                        let clocks: &[&str] = if name == "chats" { &["createdAt", "lastSeenAt"] } else { &["addedAt"] };
                        let activity_times = if name == "chats" { Some((clock(local, "lastMessageAt")?, clock(remote, "lastMessageAt")?)) }
                            else { clock(local, "addedAt")?; clock(remote, "addedAt")?; None };
                        let cached_value = row_value(cached, name, &record.key);
                        let cached_fields = cached_value.as_ref().and_then(serde_json::Value::as_object)?;
                        if crate::workspace::WORKSPACE_IDENTITY_FIELDS.iter().any(|field| cached_fields.get(*field) != local.get(*field))
                            || !crate::workspace::workspace_routes_compatible(name, [local, cached_fields]) { return None }
                        let Some(loro::ValueOrContainer::Container(loro::Container::Map(remote_row))) = doc.get_map(name).get(&record.key) else { return None };
                        let Some(loro::ValueOrContainer::Container(loro::Container::Map(cached_row))) = cached.get_map(name).get(&record.key) else { return None };
                        if remote.keys().any(|field| matches!(remote_row.get(field), Some(loro::ValueOrContainer::Container(_))))
                            || local.keys().chain(cached_fields.keys()).any(|field| matches!(cached_row.get(field), Some(loro::ValueOrContainer::Container(_)))) { return None }
                        let mut merged = remote.clone();
                        for (field, value) in local {
                            if activity_fields.contains(&field.as_str()) || remote.get(field) == Some(value) { continue }
                            if clocks.contains(&field.as_str()) && remote.contains_key(field) {
                                let (a, b) = (clock(local, field)?, clock(remote, field)?);
                                merged.insert(field.clone(), serde_json::json!(if field == "lastSeenAt" { a.max(b) } else { a.min(b) }));
                            } else {
                                // Independent seeds may add metadata, but a tombstone is not an absent field.
                                if remote.contains_key(field) || remote_row.to_handler().get_last_edit_idlp(field).is_some() { return None }
                                merged.insert(field.clone(), value.clone());
                            }
                        }
                        for field in remote.keys().filter(|field| !local.contains_key(*field)) {
                            if !activity_fields.contains(&field.as_str())
                                && cached_row.to_handler().get_last_edit_idlp(field).is_some() { return None }
                        }
                        for &field in clocks {
                            if let (Some(a), Some(b)) = (clock(&merged, field), clock(cached_fields, field)) {
                                merged.insert(field.into(), serde_json::json!(if field == "lastSeenAt" { a.max(b) } else { a.min(b) }));
                            }
                        }
                        if local.keys().chain(cached_fields.keys()).any(|field|
                            !activity_fields.contains(&field.as_str())
                            && cached_fields.get(field) != local.get(field) && cached_fields.get(field) != merged.get(field)) { return None }
                        if let Some((local_at, remote_at)) = activity_times {
                            let mut winner = if local_at > remote_at { local } else { remote };
                            if clock(cached_fields, "lastMessageAt").is_some_and(|at| at > local_at.max(remote_at)) { winner = cached_fields; }
                            // Timestamp and preview belong to one owner publication.
                            for &field in activity_fields {
                                if let Some(value) = winner.get(field) { merged.insert(field.into(), value.clone()); } else { merged.remove(field); }
                            }
                        }
                        Some(serde_json::Value::Object(merged))
                    })
                } else { None };
                let creation_merge = if seed_merge.is_some() { None } else { local_version.as_ref().and_then(|version| {
                    if record.before.is_some() { return None }
                    let (Some(local), Some(remote)) = (record.value.as_ref().and_then(serde_json::Value::as_object), remote.as_ref().and_then(serde_json::Value::as_object)) else { return None };
                    let root = doc.get_map(name);
                    let covers_edit = |source: &LoroDoc, other: &LoroDoc, edit: loro::IdLp, state_proof: bool| {
                        // Lamport bounds the peer counter, even after history compaction.
                        if version.get(&edit.peer).is_some_and(|&next| i64::from(next) > i64::from(edit.lamport)) { return true }
                        // A known uncovered source ID cannot be hidden by a counter alias.
                        source.with_oplog(|oplog| oplog.idlp_to_id(edit))
                            .map(|id| version.includes_id(id))
                            .unwrap_or_else(|| state_proof || other.with_oplog(|oplog| oplog.idlp_to_id(edit))
                                .is_some_and(|id| version.includes_id(id)))
                    };
                    // A covered document also covers state edits whose changes
                    // have been compacted out of both shallow histories.
                    let incoming_covered = version.includes_vv(candidate_version);
                    let cached_covered = version.includes_vv(cached_version);
                    let Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) = root.get(&record.key) else { return None };
                    let cached_root = cached.get_map(name);
                    let Some(loro::ValueOrContainer::Container(loro::Container::Map(cached_row))) = cached_root.get(&record.key) else { return None };
                    if cached_row.id() != row.id() { return None }
                    if !root.to_handler().get_last_edit_idlp(&record.key).is_some_and(|edit|
                        covers_edit(doc, cached, edit, incoming_covered || (cached_covered && cached_root.to_handler().get_last_edit_idlp(&record.key) == Some(edit)))) { return None }
                    let cached_handler = cached_row.to_handler();
                    let handler = row.to_handler();
                    let covers_incoming = |field: &str, edit| covers_edit(doc, cached, edit,
                        incoming_covered || (cached_covered && cached_handler.get_last_edit_idlp(field) == Some(edit)));
                    let covers_cached = |field: &str, edit| covers_edit(cached, doc, edit,
                        cached_covered || (incoming_covered && handler.get_last_edit_idlp(field) == Some(edit)));
                    // Workspace fields are atomic values; a nested container's
                    // attachment alone cannot prove coverage of its child edits.
                    if remote.keys().any(|field| matches!(row.get(field), Some(loro::ValueOrContainer::Container(_)))) { return None }
                    if remote.keys().chain(local.keys()).any(|field| matches!(cached_row.get(field), Some(loro::ValueOrContainer::Container(_)))) { return None }
                    let mut merged = remote.clone();
                    for (field, value) in local {
                        if remote.get(field) == Some(value) { continue }
                        match handler.get_last_edit_idlp(field) {
                            Some(edit) if !covers_incoming(field, edit) => return None,
                            None if remote.contains_key(field) => return None,
                            _ => {},
                        }
                        merged.insert(field.clone(), value.clone());
                    }
                    for field in remote.keys().filter(|field| !local.contains_key(*field)) {
                        match cached_handler.get_last_edit_idlp(field) {
                            None if cached_row.get(field).is_none() => {}, // Independent incoming addition.
                            Some(edit) if covers_cached(field, edit) => {
                                if cached_row.get(field).is_none() {
                                    // An explicit local tombstone wins only over an
                                    // observed field, never an unseen resurrection.
                                    if !handler.get_last_edit_idlp(field).is_some_and(|edit| covers_incoming(field, edit)) { return None }
                                    merged.remove(field);
                                }
                                // A present cached field can be a previously merged
                                // remote addition, absent from the retained intent.
                            }
                            _ => return None, // Cached imports erased the intent's field provenance.
                        }
                    }
                    let publication_follows = name == "sessions" && ["status", "startedAt", "updatedAt", "modelRetry"].iter().all(|field|
                        handler.get_last_edit_idlp(field).map(|edit| covers_incoming(field, edit)).unwrap_or(!remote.contains_key(*field)));
                    Some((serde_json::Value::Object(merged), publication_follows))
                }) };
                let recreated_after_deletion = remote.is_none() && record.value.is_some() && local_version.as_ref().is_some_and(|version| {
                    let Some(deletion) = doc.get_map(name).to_handler().get_last_edit_idlp(&record.key) else { return false };
                    let Some(deleted) = doc.with_oplog(|log| log.idlp_to_id(deletion))
                        .or_else(|| cached.with_oplog(|log| log.idlp_to_id(deletion))) else { return false };
                    let Some(created) = cached.get_map(name).to_handler().get_last_edit_idlp(&record.key)
                        .and_then(|edit| cached.with_oplog(|log| log.idlp_to_id(edit))) else { return false };
                    if !version.includes_id(created) || !version.includes_id(deleted) { return false }
                    if row_value(cached, name, &record.key) != record.value { return false }
                    // The insertion itself must have observed the tombstone; unrelated room edits
                    // and later cache imports cannot turn a concurrent resurrection into a recreation.
                    cached.inner().frontiers_to_vv(&created.into()).is_some_and(|observed| observed.includes_id(deleted))
                });
                let local_follows_remote = creation_merge.as_ref().is_some_and(|(_, follows)| *follows)
                    || recreated_after_deletion || local_version.as_ref().is_some_and(|version| version.includes_vv(candidate_version));
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
                        ["status", "startedAt", "updatedAt", "modelRetry"].iter().all(|field| row.get(*field) == remote.get(*field)));
                    if before.is_some_and(|row| row["updatedAt"].as_i64().is_some_and(|at| local_at < at || remote_at < at))
                        || (!local_publication_wins_tie && local_at == remote_at && ["status", "startedAt", "modelRetry"].iter().any(|field| local.get(*field) != remote.get(*field)))
                    { return Err(conflict()) }
                    // Status and run identity belong to the clocked publication, never to separate field winners.
                    let mut publication = if local_at > remote_at || (local_at == remote_at && local_publication_wins_tie) { local } else { remote };
                    if let Some(cached) = cached {
                        let cached_at = cached["updatedAt"].as_i64().expect("validated owner clock");
                        let at = publication["updatedAt"].as_i64().expect("validated owner clock");
                        if cached_at == at && ["status", "startedAt", "modelRetry"].iter().any(|field| cached.get(*field) != publication.get(*field)) { return Err(conflict()) }
                        if cached_at > at { publication = cached; }
                    }
                    Some(publication)
                } else { None };
                let mut merged = if let Some(value) = seed_merge {
                    Some(value)
                } else if let Some((value, _)) = creation_merge {
                    Some(value)
                } else if local_follows_remote && (record.before.is_none() || record.value.is_none() || remote.is_none()) {
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
                        let cached_value = std::cell::LazyCell::new(|| row_value(cached, name, &record.key));
                        let clock = |row: &serde_json::Map<String, serde_json::Value>, field: &str| row.get(field).and_then(serde_json::Value::as_i64)
                            .filter(|at| chrono::DateTime::from_timestamp_millis(*at).is_some());
                        let mut activity_merged = false;
                        let chat_identity = name == "chats"
                            && local.get("id").and_then(serde_json::Value::as_str) == Some(record.key.as_str())
                            && remote.get("id") == local.get("id") && before.and_then(|row| row.get("deviceId")) == local.get("deviceId")
                            && local.get("deviceId").and_then(serde_json::Value::as_str).is_some_and(|owner| !owner.is_empty() && remote.get("deviceId").and_then(serde_json::Value::as_str) == Some(owner));
                        if chat_identity
                            && let (Some(local_at), Some(remote_at)) = (clock(local, "lastMessageAt"), clock(remote, "lastMessageAt"))
                            && before.and_then(|row| clock(row, "lastMessageAt")).is_none_or(|at| local_at >= at && remote_at >= at)
                        {
                            let local_wins_tie = local_follows_remote || before.is_some_and(|row|
                                ["lastMessageAt", "lastMessagePreview"].iter().all(|field| row.get(*field) == remote.get(*field)));
                            let mut winner = if local_at > remote_at || (local_at == remote_at && local_wins_tie) { local } else { remote };
                            if let Some(row) = cached_value.as_ref().and_then(serde_json::Value::as_object)
                                && crate::workspace::WORKSPACE_IDENTITY_FIELDS.iter().all(|field| row.get(*field) == local.get(*field))
                                && crate::workspace::workspace_routes_compatible(name, [local, row])
                                && clock(row, "lastMessageAt").is_some_and(|at| at > local_at.max(remote_at)) { winner = row; }
                            // The preview and its owner clock are one observation, not competing user edits.
                            for field in ["lastMessageAt", "lastMessagePreview"] {
                                if let Some(value) = winner.get(field) { merged.insert(field.into(), value.clone()); } else { merged.remove(field); }
                            }
                            activity_merged = true;
                        }
                        for field in before.into_iter().flat_map(|row| row.keys()).chain(local.keys()).collect::<BTreeSet<_>>() {
                            if owner_publication.is_some() && matches!(field.as_str(), "status" | "startedAt" | "updatedAt" | "modelRetry") { continue }
                            if activity_merged && matches!(field.as_str(), "lastMessageAt" | "lastMessagePreview") { continue }
                            if chat_identity && field == "lastSeenAt"
                                && let (Some(local_at), Some(remote_at)) = (clock(local, field), clock(remote, field))
                                && before.is_none_or(|row| !row.contains_key(field) || clock(row, field).is_some_and(|at| local_at >= at && remote_at >= at))
                                && let Some(row) = cached_value.as_ref().and_then(serde_json::Value::as_object)
                                && crate::workspace::WORKSPACE_IDENTITY_FIELDS.iter().all(|field| row.get(*field) == local.get(*field))
                                && crate::workspace::workspace_routes_compatible(name, [local, row])
                                && let Some(cached_at) = clock(row, field)
                                && before.and_then(|row| clock(row, field)).is_none_or(|at| cached_at >= at)
                            {
                                // Concurrent reads advance one observation. Absence, deletion and
                                // a backwards clock may encode an explicit unread/read transition.
                                merged.insert(field.clone(), serde_json::json!(local_at.max(remote_at).max(cached_at)));
                                continue;
                            }
                            if before.and_then(|row| row.get(field)) == local.get(field) { continue }
                            if !local_follows_remote && remote.get(field) != before.and_then(|row| row.get(field)) && remote.get(field) != local.get(field) {
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
                    for field in ["status", "startedAt", "updatedAt", "modelRetry"] {
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
    fn self_alias_journal_recovery_survives_restart_and_keeps_identity_fences() {
        let public = "018eeb58-6508-78e8-a544-44682ab94c50";
        let alias = format!("{public}::session::{public}::session::{public}");
        let remote = LoroDoc::new();
        let row = remote.get_map("chats").insert_container(public, LoroMap::new()).unwrap();
        row.insert("id", public).unwrap(); row.insert("deviceId", "owner").unwrap(); row.insert("title", "before").unwrap(); remote.commit();
        let before = serde_json::json!({"id": alias, "deviceId": "owner", "title": "before"});
        let after = serde_json::json!({"id": public, "deviceId": "owner", "title": "offline"});
        let binding = SharedDocument::new(LoroDoc::new());
        binding.install_journal(vec![PendingRecord { container: "chats".into(), key: alias.clone(), before: Some(before), value: Some(after), version: Vec::new(), acknowledged: false }], Arc::new(|_,_| Ok(())));
        binding.adopt_snapshot(&remote.export(ExportMode::Snapshot).unwrap(), None).unwrap();
        assert_eq!(binding.get_map("chats").get_deep_value().to_json_value()[public]["title"], "offline");
        assert!(binding.get_map("chats").get(&alias).is_none());
        let records = binding.pending_records();
        assert_eq!(records[0].key, public);
        assert_eq!(records[0].before.as_ref().unwrap()["id"], public);
        let restarted = SharedDocument::new(LoroDoc::new());
        restarted.install_journal(records.clone(), Arc::new(|_,_| Ok(())));
        restarted.adopt_snapshot(&binding.export(ExportMode::Snapshot).unwrap(), None).unwrap();
        assert_eq!(restarted.get_deep_value(), binding.get_deep_value());
        for (field, value) in [("deviceId", "foreign"), ("projectId", "foreign-project"), ("deploymentId", "foreign-deployment")] {
            let foreign = remote.fork();
            let row = foreign.get_map("chats").get(public).unwrap().get_deep_value().to_json_value();
            let mut row = row.as_object().unwrap().clone(); row.insert(field.into(), value.into());
            foreign.get_map("chats").insert(public, LoroValue::from(serde_json::Value::Object(row))).unwrap(); foreign.commit();
            assert!(binding.adopt_snapshot(&foreign.export(ExportMode::Snapshot).unwrap(), None).is_err());
        }
        let deletion = LoroDoc::new(); deletion.import(&binding.export(ExportMode::Snapshot).unwrap()).unwrap();
        deletion.get_map("chats").delete(public).unwrap(); deletion.commit();
        binding.adopt_snapshot(&deletion.export(ExportMode::Snapshot).unwrap(), None).unwrap();
        assert!(binding.get_map("chats").get(public).is_none());
    }

    #[test]
    fn covered_nested_alias_bootstrap_adopts_canonical_observations_and_restarts() {
        let public = "018eeb58-6508-78e8-a544-44682ab94c50";
        let alias = format!("{public}::session::{public}::session::{public}");
        let cached = LoroDoc::new();
        for (key, clock) in [(public, 200_i64), (alias.as_str(), 100_i64)] {
            let row = cached.get_map("chats").insert_container(key, LoroMap::new()).unwrap();
            row.insert("id", key).unwrap(); row.insert("deviceId", "owner").unwrap(); row.insert("title", "retained user title").unwrap();
            row.insert("lastSeenAt", clock).unwrap(); row.insert("lastMessageAt", clock).unwrap();
        }
        cached.commit();
        let records: Vec<_> = [public, alias.as_str()].into_iter().map(|key| PendingRecord { container: "chats".into(), key: key.into(), before: None,
            value: row_value(&cached, "chats", key), version: cached.oplog_vv().encode(), acknowledged: false }).collect();
        let binding = SharedDocument::new(cached.fork());
        binding.install_journal(records.clone(), Arc::new(|_,_| Ok(())));
        binding.adopt_snapshot(&cached.export(ExportMode::Snapshot).unwrap(), None).unwrap();
        assert_eq!(binding.pending_records().len(), 1);
        assert_eq!(binding.get_map("chats").get_deep_value().to_json_value()[public]["lastSeenAt"], 200);
        assert!(binding.get_map("chats").get(&alias).is_none());
        let restored = SharedDocument::new(binding.raw().fork());
        restored.install_journal(binding.pending_records(), Arc::new(|_,_| Ok(())));
        restored.adopt_snapshot(&binding.export(ExportMode::Snapshot).unwrap(), None).unwrap();
        assert_eq!(restored.get_deep_value(), binding.get_deep_value());
        let mut unknown = records; unknown[1].version.clear();
        let blocked = SharedDocument::new(cached.fork());
        blocked.install_journal(unknown, Arc::new(|_,_| Ok(())));
        assert!(blocked.adopt_snapshot(&cached.export(ExportMode::Snapshot).unwrap(), None).is_err());
        assert!(blocked.get_map("chats").get(&alias).is_some());
    }

    #[test]
    fn membership_first_route_survives_recovery_restart_and_rejects_known_route_changes() {
        let key = "5:owner:opaque:membership";
        let remote = LoroDoc::new();
        let row = remote.get_map("sessionRefs").insert_container(key, LoroMap::new()).unwrap();
        row.insert("userId", "owner").unwrap(); row.insert("chatId", "opaque:membership").unwrap(); row.insert("addedAt", 1_i64).unwrap(); remote.commit();
        let environment = serde_json::json!({"ownerPrincipal": "session-owner", "scope": {"projectId": "project", "deploymentId": "deployment", "sessionId": "opaque-route"}, "source": {"kind": "scaffold"}});
        let binding = SharedDocument::new(remote.fork()); journal(&binding);
        let Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) = binding.get_map("sessionRefs").get(key) else { panic!("missing membership") };
        row.insert("environment", LoroValue::from(environment.clone())).unwrap(); binding.commit().unwrap();
        binding.adopt_snapshot(&remote.export(ExportMode::Snapshot).unwrap(), None).unwrap();
        assert_eq!(row_value(&binding.raw(), "sessionRefs", key).unwrap()["environment"], environment);
        let restored = SharedDocument::new(binding.raw().fork());
        restored.install_journal(binding.pending_records(), Arc::new(|_,_| Ok(())));
        restored.adopt_snapshot(&binding.export(ExportMode::Snapshot).unwrap(), None).unwrap();
        assert_eq!(row_value(&restored.raw(), "sessionRefs", key).unwrap()["environment"], environment);
        for field in ["projectId", "deploymentId", "ownerPrincipal", "userId"] {
            let foreign = restored.raw().fork();
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) = foreign.get_map("sessionRefs").get(key) else { panic!("missing membership") };
            if field == "userId" { row.insert(field, "foreign").unwrap(); }
            else {
                let mut changed = environment.clone();
                if field == "ownerPrincipal" { changed[field] = "foreign".into(); }
                else { changed["scope"][field] = "foreign".into(); }
                row.insert("environment", LoroValue::from(changed)).unwrap();
            }
            foreign.commit();
            assert!(restored.adopt_snapshot(&foreign.export(ExportMode::Snapshot).unwrap(), None).is_err());
            assert_eq!(row_value(&restored.raw(), "sessionRefs", key).unwrap()["environment"], environment);
        }
        let deletion = remote.fork(); deletion.get_map("sessionRefs").delete(key).unwrap(); deletion.commit();
        assert!(restored.adopt_snapshot(&deletion.export(ExportMode::Snapshot).unwrap(), None).is_err());
        assert!(deletion.get_map("sessionRefs").get(key).is_none());
    }

    #[test]
    fn independent_membership_seeds_keep_first_route_and_reject_unseen_removal() {
        let key = "5:owner:chat";
        let local = SharedDocument::new(LoroDoc::new()); journal(&local);
        let row = local.get_map("sessionRefs").insert_container(key, LoroMap::new()).unwrap();
        row.insert("userId", "owner").unwrap(); row.insert("chatId", "chat").unwrap(); row.insert("addedAt", 100i64).unwrap();
        let environment = serde_json::json!({"ownerPrincipal":"owner", "scope":{"projectId":"project", "deploymentId":"deployment", "sessionId":"chat"}});
        row.insert("environment", LoroValue::from(environment.clone())).unwrap(); row.insert("startup", "accepted startup").unwrap(); local.commit().unwrap();
        let original = local.pending_records()[0].clone();
        let original_snapshot = local.export(ExportMode::Snapshot).unwrap();
        let remote = LoroDoc::new();
        let row = remote.get_map("sessionRefs").insert_container(key, LoroMap::new()).unwrap();
        row.insert("userId", "owner").unwrap(); row.insert("chatId", "chat").unwrap(); row.insert("addedAt", 200i64).unwrap(); remote.commit();
        let incoming = remote.export(ExportMode::shallow_snapshot(&remote.state_frontiers())).unwrap();
        for _ in 0..2 {
            local.adopt_snapshot(&incoming, None).unwrap();
            let row = row_value(&local.raw(), "sessionRefs", key).unwrap();
            assert_eq!(row["environment"], environment);
            assert_eq!(row["startup"], "accepted startup");
            assert_eq!(row["addedAt"], 100);
            assert_eq!(local.pending_records()[0].value, original.value);
        }
        let cached = LoroDoc::new(); cached.import(&original_snapshot).unwrap();
        let blocked = SharedDocument::new(cached);
        blocked.install_journal(vec![original], Arc::new(|_, _| Ok(())));
        let preserved = blocked.export(ExportMode::Snapshot).unwrap();
        let retained = serde_json::to_vec(&blocked.pending_records()).unwrap();
        row.insert("environment", LoroValue::from(environment)).unwrap(); row.delete("environment").unwrap(); remote.commit();
        assert!(blocked.adopt_snapshot(&remote.export(ExportMode::shallow_snapshot(&remote.state_frontiers())).unwrap(), None).is_err());
        remote.get_map("sessionRefs").delete(key).unwrap(); remote.commit();
        assert!(blocked.adopt_snapshot(&remote.export(ExportMode::Snapshot).unwrap(), None).is_err());
        assert_eq!(blocked.export(ExportMode::Snapshot).unwrap(), preserved);
        assert_eq!(serde_json::to_vec(&blocked.pending_records()).unwrap(), retained);
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
    fn known_deletion_allows_causal_recreation_but_not_concurrent_resurrection() {
        for observed in [true, false] {
            let remote = LoroDoc::new();
            let stage = |doc: &LoroDoc, deadline: i64| {
                let row = doc.get_map("worktreeDeletions").insert_container("chat", LoroMap::new()).unwrap();
                row.insert("chatId", "chat").unwrap(); row.insert("ownerSubject", "owner").unwrap();
                row.insert("ownerDeviceId", "device").unwrap(); row.insert("path", "/accepted/worktree").unwrap();
                row.insert("deleteAfter", deadline).unwrap(); doc.commit();
            };
            stage(&remote, 1_000);
            let initial = remote.export(ExportMode::Snapshot).unwrap();
            remote.get_map("worktreeDeletions").delete("chat").unwrap(); remote.commit();
            let deleted = remote.export(ExportMode::Snapshot).unwrap();
            let raw = LoroDoc::new(); raw.import(if observed { &deleted } else { &initial }).unwrap();
            let local = SharedDocument::new(raw.clone()); journal(&local);
            if !observed { raw.get_map("worktreeDeletions").delete("chat").unwrap(); raw.commit(); }
            stage(&raw, 2_000);
            // A later import/repair clock cannot retroactively make an unseen cancellation observed.
            raw.import(&deleted).unwrap();
            let mut records = local.pending_records();
            records[0].version = raw.oplog_vv().encode();
            drop(local);
            let local = SharedDocument::new(raw); local.install_journal(records.clone(), Arc::new(|_, _| Ok(())));
            let peer = remote.get_map("devices").insert_container("peer", LoroMap::new()).unwrap();
            peer.insert("id", "peer").unwrap(); peer.insert("name", "unrelated heartbeat").unwrap(); remote.commit();
            assert!(!loro::VersionVector::decode(&records[0].version).unwrap().includes_vv(&remote.oplog_vv()));
            let incoming = remote.export(ExportMode::shallow_snapshot(&remote.state_frontiers())).unwrap();
            let preserved = local.export(ExportMode::Snapshot).unwrap();
            let retained = serde_json::to_vec(&local.pending_records()).unwrap();
            if observed {
                for _ in 0..2 {
                    local.adopt_snapshot(&incoming, None).unwrap();
                    assert_eq!(row_value(&local.raw(), "worktreeDeletions", "chat").unwrap()["deleteAfter"], 2_000);
                    assert_eq!(local.pending_records()[0].before, records[0].before);
                    assert_eq!(local.pending_records()[0].value, records[0].value);
                    assert_eq!(row_value(&local.raw(), "devices", "peer").unwrap()["name"], "unrelated heartbeat");
                }
            } else {
                assert!(local.adopt_snapshot(&incoming, None).is_err());
                assert_eq!(local.export(ExportMode::Snapshot).unwrap(), preserved);
                assert_eq!(serde_json::to_vec(&local.pending_records()).unwrap(), retained);
            }
        }
    }

    #[test]
    fn older_workspace_checkpoint_preserves_descendant_edits_and_unblocks_writes() {
        let local = WorkspaceDoc::new();
        local.doc().set_peer_id(1).unwrap();
        for index in 0..64 { local.doc().get_map("devices").insert("warmup", index).unwrap(); }
        local.doc().commit();
        local.doc().set_peer_id(2).unwrap();
        let original = local.binding(); journal(&original);
        let row = original.get_map("chats").insert_container("chat", LoroMap::new()).unwrap();
        row.insert("id", "chat").unwrap(); row.insert("deviceId", "owner").unwrap();
        row.insert("archived", false).unwrap(); row.insert("lastSeenAt", 1i64).unwrap(); original.commit().unwrap();
        let remote = LoroDoc::new(); remote.import(&local.export_snapshot().unwrap()).unwrap();
        remote.set_peer_id(3).unwrap(); remote.get_map("devices").insert("observer", "observed").unwrap(); remote.commit();
        row.insert("archived", true).unwrap(); row.delete("lastSeenAt").unwrap(); original.commit().unwrap();
        original.import(&remote.export(ExportMode::Snapshot).unwrap()).unwrap();
        let records = original.pending_records();
        let compacted = original.export(ExportMode::shallow_snapshot(&original.raw().state_frontiers())).unwrap();
        let raw = LoroDoc::new(); raw.import(&compacted).unwrap();
        let restored = SharedDocument::new(raw);
        restored.install_journal(records.clone(), Arc::new(|_, _| Ok(())));
        restored.fail_recovery("prior checkpoint conflict".into());
        let checkpoint = remote.export(ExportMode::shallow_snapshot(&remote.state_frontiers())).unwrap();
        restored.adopt_snapshot(&checkpoint, None).unwrap();
        assert!(restored.recovery_error().is_none());
        let chat = row_value(&restored.raw(), "chats", "chat").unwrap();
        assert_eq!(chat["archived"], true);
        assert!(chat.get("lastSeenAt").is_none());
        assert_eq!(restored.pending_records()[0].value, records[0].value);
        assert!(!restored.pending_records()[0].acknowledged);
        restored.get_map("chats").insert("new-session", LoroValue::from(serde_json::json!({"id": "new-session", "deviceId": "owner", "archived": false}))).unwrap();
        restored.commit().unwrap();
        assert_eq!(row_value(&restored.raw(), "chats", "new-session").unwrap()["archived"], false);
        let snapshot = restored.export(ExportMode::Snapshot).unwrap();
        let retained = serde_json::to_vec(&restored.pending_records()).unwrap();
        let foreign = remote.fork();
        foreign.set_peer_id(4).unwrap();
        let loro::ValueOrContainer::Container(loro::Container::Map(row)) = foreign.get_map("chats").get("chat").unwrap() else { panic!("chat missing") };
        row.insert("deviceId", "foreign-owner").unwrap(); foreign.commit();
        assert!(restored.adopt_snapshot(&foreign.export(ExportMode::Snapshot).unwrap(), None).is_err());
        assert_eq!(restored.export(ExportMode::Snapshot).unwrap(), snapshot);
        assert_eq!(serde_json::to_vec(&restored.pending_records()).unwrap(), retained);
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
    fn alias_normalization_preserves_causal_proof_for_compacted_creation_intents() {
        let source = LoroDoc::new();
        source.set_peer_id(3).unwrap();
        let clock = source.get_map("devices").insert_container("clock", LoroMap::new()).unwrap();
        for tick in 0..64 { clock.insert("tick", tick).unwrap(); source.commit(); }
        source.set_peer_id(1).unwrap();
        let row = source.get_map("chats").insert_container("chat", LoroMap::new()).unwrap();
        row.insert("id", "chat").unwrap(); row.insert("deviceId", "owner").unwrap();
        row.insert("title", "before").unwrap(); source.commit();
        let public = "018eeb58-6508-78e8-a544-44682ab94c50";
        let alias = format!("{public}::session::{public}");
        let legacy = source.get_map("chats").insert_container(&alias, LoroMap::new()).unwrap();
        legacy.insert("id", alias.as_str()).unwrap(); legacy.insert("deviceId", "owner").unwrap();
        legacy.insert("title", "legacy session").unwrap(); source.commit();
        let snapshot = source.export(ExportMode::shallow_snapshot(&source.state_frontiers())).unwrap();
        let cached = source.fork(); cached.set_peer_id(2).unwrap();
        let loro::ValueOrContainer::Container(loro::Container::Map(row)) = cached.get_map("chats").get("chat").unwrap() else { panic!("missing chat") };
        row.insert("title", "accepted offline rename").unwrap(); cached.commit();
        let original = PendingRecord { container: "chats".into(), key: "chat".into(), before: None,
            value: row_value(&cached, "chats", "chat"), version: cached.oplog_vv().encode(), acknowledged: false };
        // Later unrelated cache activity must not erase the retained intent's proof.
        cached.get_map("devices").insert("later", true).unwrap(); cached.commit();
        let raw = LoroDoc::new();
        raw.import(&cached.export(ExportMode::shallow_snapshot(&cached.state_frontiers())).unwrap()).unwrap();
        let binding = SharedDocument::new(raw);
        binding.install_journal(vec![original.clone()], Arc::new(|_, _| Ok(())));
        for _ in 0..2 {
            binding.adopt_snapshot(&snapshot, None).unwrap();
            assert_eq!(row_value(&binding.raw(), "chats", "chat").unwrap()["title"], "accepted offline rename");
            assert_eq!(row_value(&binding.raw(), "chats", public).unwrap()["title"], "legacy session");
            assert!(binding.get_map("chats").get(&alias).is_none());
            assert_eq!(binding.pending_records()[0].value, original.value);
        }
        let preserved = binding.export(ExportMode::Snapshot).unwrap();
        let intents = serde_json::to_vec(&binding.pending_records()).unwrap();
        for (field, value) in [("title", "unseen competing rename"), ("deviceId", "foreign-owner")] {
            let foreign = LoroDoc::new(); foreign.import(&snapshot).unwrap();
            let loro::ValueOrContainer::Container(loro::Container::Map(row)) = foreign.get_map("chats").get("chat").unwrap() else { panic!("missing chat") };
            row.insert(field, value).unwrap(); foreign.commit();
            assert!(binding.adopt_snapshot(&foreign.export(ExportMode::Snapshot).unwrap(), None).is_err());
            assert_eq!(binding.export(ExportMode::Snapshot).unwrap(), preserved);
            assert_eq!(serde_json::to_vec(&binding.pending_records()).unwrap(), intents);
        }
    }

    #[test]
    fn same_peer_unrelated_edits_preserve_cold_creation_and_row_provenance() {
        for (shallow, warmup) in [(false, 0), (true, 0), (false, 64), (true, 64)] {
            let local = WorkspaceDoc::new();
            local.doc().set_peer_id(3).unwrap();
            if warmup > 0 {
                let clock = local.doc().get_map("devices").insert_container("clock", LoroMap::new()).unwrap();
                clock.insert("id", "clock").unwrap(); clock.insert("name", "clock").unwrap();
                for tick in 0..warmup { clock.insert("tick", tick).unwrap(); local.doc().commit(); }
            }
            local.doc().set_peer_id(1).unwrap();
            let original_binding = local.binding(); journal(&original_binding);
            let row = original_binding.get_map("chats").insert_container("chat", LoroMap::new()).unwrap();
            row.insert("id", "chat").unwrap(); row.insert("deviceId", "owner-device").unwrap();
            row.insert("title", "created").unwrap(); row.insert("removeMe", "observed field").unwrap(); original_binding.commit().unwrap();
            let raw = LoroDoc::new(); raw.import(&local.export_snapshot().unwrap()).unwrap();
            raw.set_peer_id(1).unwrap();
            let remote = WorkspaceDoc::from_doc(raw);
            // A restarted owner keeps the coalesced creation intent, but uses a
            // fresh Loro peer for its accepted rename (no same-peer history fork).
            local.doc().set_peer_id(2).unwrap();
            local.rename_chat("chat", "accepted offline rename").unwrap();
            row.delete("removeMe").unwrap(); original_binding.commit().unwrap();
            let original = original_binding.pending_records();
            assert_eq!(original.len(), 1); assert!(original[0].before.is_none());
            let raw = LoroDoc::new(); raw.import(&local.export_snapshot().unwrap()).unwrap();
            let mut restarted = WorkspaceDoc::from_doc(raw);
            let mut binding = restarted.binding();
            binding.install_journal(original.clone(), Arc::new(|_,_| Ok(())));
            let device = remote.doc().get_map("devices").insert_container("peer", LoroMap::new()).unwrap();
            device.insert("id", "peer").unwrap(); device.insert("name", "unrelated heartbeat").unwrap();
            let loro::ValueOrContainer::Container(loro::Container::Map(remote_row)) = remote.doc().get_map("chats").get("chat").unwrap() else { panic!("chat row missing"); };
            remote_row.insert("remoteNote", "independent field must survive").unwrap();
            remote.doc().commit();
            let accepted = loro::VersionVector::decode(&original[0].version).unwrap();
            assert!(remote.doc().oplog_vv().get(&1) > accepted.get(&1));
            let snapshot = if shallow {
                remote.doc().export(ExportMode::shallow_snapshot(&remote.doc().state_frontiers())).unwrap()
            } else { remote.export_snapshot().unwrap() };
            for recovery in 0..3 {
                binding.adopt_snapshot(&snapshot, None).unwrap_or_else(|error| panic!("shallow={shallow} recovery={recovery}: {error}"));
                assert_eq!(restarted.chat("chat").unwrap().unwrap().title.as_deref(), Some("accepted offline rename"));
                assert_eq!(binding.get_map("devices").get("peer").unwrap().get_deep_value().to_json_value()["name"], "unrelated heartbeat");
                let row = binding.get_map("chats").get("chat").unwrap().get_deep_value().to_json_value();
                assert_eq!(row["remoteNote"], "independent field must survive");
                assert!(row.get("removeMe").is_none());
                assert_eq!(binding.pending_records()[0].before, original[0].before);
                assert_eq!(binding.pending_records()[0].value, original[0].value);
                if recovery == 1 {
                    let records = binding.pending_records();
                    let raw = LoroDoc::new(); raw.import(&restarted.export_snapshot().unwrap()).unwrap();
                    restarted = WorkspaceDoc::from_doc(raw);
                    binding = restarted.binding();
                    binding.install_journal(records, Arc::new(|_,_| Ok(())));
                }
            }
            let preserved = restarted.export_snapshot().unwrap();
            let intents = serde_json::to_vec(&binding.pending_records()).unwrap();
            for change in ["title", "deviceId", "field deletion", "row deletion", "row replacement", "resurrection", "nested container"] {
                let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
                raw.set_peer_id(1).unwrap();
                let changed = WorkspaceDoc::from_doc(raw);
                let root = changed.doc().get_map("chats");
                let loro::ValueOrContainer::Container(loro::Container::Map(row)) = root.get("chat").unwrap() else { panic!("chat row missing"); };
                match change {
                    "title" => row.insert("title", "conflicting rename").unwrap(),
                    "deviceId" => row.insert("deviceId", "foreign-owner").unwrap(),
                    "field deletion" => row.delete("title").unwrap(),
                    "row deletion" => root.delete("chat").unwrap(),
                    "row replacement" => {
                        root.delete("chat").unwrap();
                        let row = root.insert_container("chat", LoroMap::new()).unwrap();
                        row.insert("id", "chat").unwrap(); row.insert("deviceId", "owner-device").unwrap();
                        row.insert("title", "created").unwrap();
                    }
                    "resurrection" => row.insert("removeMe", "unseen resurrection").unwrap(),
                    _ => { row.insert_container("nested", LoroMap::new()).unwrap(); }
                }
                changed.doc().commit();
                assert!(binding.adopt_snapshot(&changed.export_snapshot().unwrap(), None).is_err(), "{change}");
                assert_eq!(restarted.export_snapshot().unwrap(), preserved, "{change}");
                assert_eq!(serde_json::to_vec(&binding.pending_records()).unwrap(), intents, "{change}");
            }
            // A later cache import can overwrite the deletion's winning metadata.
            // Its uncovered present field is ambiguous, not an independent addition.
            for update in 0..8 { device.insert("name", format!("later heartbeat {update}")).unwrap(); }
            remote_row.insert("removeMe", "unseen resurrection").unwrap(); remote.doc().commit();
            let resurrection = remote.export_snapshot().unwrap();
            binding.raw().import(&resurrection).unwrap();
            assert_eq!(binding.get_map("chats").get("chat").unwrap().get_deep_value().to_json_value()["removeMe"], "unseen resurrection");
            let cached_import = restarted.export_snapshot().unwrap();
            assert!(binding.adopt_snapshot(&resurrection, None).is_err());
            assert_eq!(restarted.export_snapshot().unwrap(), cached_import);
            assert_eq!(serde_json::to_vec(&binding.pending_records()).unwrap(), intents);
        }
    }

    #[test]
    fn coalesced_creation_activity_keeps_clock_and_preview_across_independent_seed() {
        for (local_at, remote_at, sparse) in [(3_000, 2_000, false), (2_000, 3_000, false), (3_000, 2_000, true), (2_000, 3_000, true)] {
            let local = WorkspaceDoc::new();
            let binding = local.binding(); journal(&binding);
            let row = binding.get_map("chats").insert_container("chat", LoroMap::new()).unwrap();
            row.insert("id", "chat").unwrap(); row.insert("deviceId", "owner-device").unwrap();
            row.insert("title", "retained creation").unwrap(); binding.commit().unwrap();
            if sparse {
                row.insert("cwd", "/accepted/worktree").unwrap();
                row.insert("config", LoroValue::from(serde_json::json!({"model":"accepted-model"}))).unwrap();
                row.insert("createdAt", 100i64).unwrap(); row.insert("lastSeenAt", 4_000i64).unwrap(); binding.commit().unwrap();
            }
            local.set_chat_last_message("chat", "local preview", chrono::DateTime::from_timestamp_millis(local_at).unwrap()).unwrap();
            let original = binding.pending_records()[0].clone();
            let original_snapshot = local.export_snapshot().unwrap();
            assert!(original.before.is_none());
            let remote = WorkspaceDoc::new();
            let row = remote.doc().get_map("chats").insert_container("chat", LoroMap::new()).unwrap();
            row.insert("id", "chat").unwrap(); row.insert("deviceId", "owner-device").unwrap();
            if sparse { row.insert("createdAt", 200i64).unwrap(); } else { row.insert("title", "retained creation").unwrap(); }
            remote.doc().commit();
            remote.set_chat_last_message("chat", "remote preview", chrono::DateTime::from_timestamp_millis(remote_at).unwrap()).unwrap();
            let snapshot = remote.export_snapshot().unwrap();
            for _ in 0..2 {
                binding.adopt_snapshot(&snapshot, None).unwrap();
                let row = binding.get_map("chats").get("chat").unwrap().get_deep_value().to_json_value();
                assert_eq!(row["lastMessageAt"], 3_000);
                assert_eq!(row["lastMessagePreview"], if local_at > remote_at { "local preview" } else { "remote preview" });
                assert_eq!(row["title"], "retained creation");
                if sparse {
                    assert_eq!(row["cwd"], "/accepted/worktree");
                    assert_eq!(row["config"]["model"], "accepted-model");
                    assert_eq!(row["createdAt"], 100);
                    assert_eq!(row["lastSeenAt"], 4_000);
                }
                assert_eq!(binding.pending_records()[0].before, original.before);
                assert_eq!(binding.pending_records()[0].value, original.value);
            }
            for field in ["title", "lastSeenAt"] {
                if field == "lastSeenAt" && !sparse { continue }
                let conflicting = LoroDoc::new(); conflicting.import(&snapshot).unwrap();
                let loro::ValueOrContainer::Container(loro::Container::Map(row)) = conflicting.get_map("chats").get("chat").unwrap() else { panic!("missing chat"); };
                row.insert(field, "explicit deletion").unwrap(); row.delete(field).unwrap(); conflicting.commit();
                let deleted = conflicting.export(ExportMode::shallow_snapshot(&conflicting.state_frontiers())).unwrap();
                let raw = LoroDoc::new(); raw.import(&original_snapshot).unwrap();
                let blocked = SharedDocument::new(raw);
                blocked.install_journal(vec![original.clone()], Arc::new(|_, _| Ok(())));
                let preserved = blocked.export(ExportMode::Snapshot).unwrap();
                let retained = serde_json::to_vec(&blocked.pending_records()).unwrap();
                assert!(blocked.adopt_snapshot(&deleted, None).is_err());
                assert_eq!(blocked.export(ExportMode::Snapshot).unwrap(), preserved);
                assert_eq!(serde_json::to_vec(&blocked.pending_records()).unwrap(), retained);
            }
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
    fn seen_recovery_merges_observations_but_preserves_explicit_unread() {
        let at = |ms| chrono::DateTime::from_timestamp_millis(ms).unwrap();
        for (local_at, remote_at) in [(3_000, 2_000), (2_000, 3_000), (2_000, 2_000)] {
            let remote = WorkspaceDoc::new();
            let row = remote.doc().get_map("chats").insert_container("chat", LoroMap::new()).unwrap();
            row.insert("id", "chat").unwrap(); row.insert("deviceId", "owner").unwrap();
            remote.doc().commit(); remote.set_chat_seen("chat", at(1_000)).unwrap();
            let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
            let local = WorkspaceDoc::from_doc(raw); let binding = local.binding(); journal(&binding);
            local.set_chat_seen("chat", at(local_at)).unwrap();
            let original = binding.pending_records()[0].clone();
            remote.set_chat_seen("chat", at(remote_at)).unwrap();
            remote.rename_chat("chat", "remote title").unwrap();
            let incoming = remote.doc().export(ExportMode::shallow_snapshot(&remote.doc().state_frontiers())).unwrap();
            binding.fail_recovery("retained seen-marker conflict".into());
            for _ in 0..2 {
                binding.adopt_snapshot(&incoming, None).unwrap();
                assert_eq!(local.chat("chat").unwrap().unwrap().last_seen_at, Some(at(local_at.max(remote_at))));
                assert_eq!(local.chat("chat").unwrap().unwrap().title.as_deref(), Some("remote title"));
            }
            remote.set_chat_seen("chat", at(4_000)).unwrap();
            binding.adopt_snapshot(&remote.export_snapshot().unwrap(), None).unwrap();
            let raw = LoroDoc::new(); raw.import(&local.export_snapshot().unwrap()).unwrap();
            let restarted = WorkspaceDoc::from_doc(raw); let rebound = restarted.binding();
            rebound.install_journal(binding.pending_records(), Arc::new(|_, _| Ok(())));
            rebound.adopt_snapshot(&incoming, None).unwrap();
            assert_eq!(restarted.chat("chat").unwrap().unwrap().last_seen_at, Some(at(4_000)));
            assert_eq!(rebound.pending_records()[0].before, original.before);
            assert_eq!(rebound.pending_records()[0].value, original.value);
            restarted.rename_chat("chat", "writes admitted after recovery").unwrap();

            for local_unread in [false, true] {
                let raw = LoroDoc::new(); raw.import(&incoming).unwrap();
                let left = WorkspaceDoc::from_doc(raw); journal(&left.binding());
                let raw = LoroDoc::new(); raw.import(&incoming).unwrap();
                let right = WorkspaceDoc::from_doc(raw);
                if local_unread { left.set_chat_unread("chat").unwrap(); right.set_chat_seen("chat", at(5_000)).unwrap(); }
                else { left.set_chat_seen("chat", at(5_000)).unwrap(); right.set_chat_unread("chat").unwrap(); }
                let preserved = left.export_snapshot().unwrap();
                let retained = serde_json::to_vec(&left.binding().pending_records()).unwrap();
                assert!(left.binding().adopt_snapshot(&right.export_snapshot().unwrap(), None).is_err());
                assert_eq!(left.export_snapshot().unwrap(), preserved);
                assert_eq!(serde_json::to_vec(&left.binding().pending_records()).unwrap(), retained);
            }
        }
    }

    #[test]
    fn chat_preview_recovery_keeps_the_latest_observation_and_original_intent() {
        for (local_at, remote_at) in [(3_000, 2_000), (2_000, 3_000), (2_000, 2_000)] {
            let remote = WorkspaceDoc::new();
            let row = remote.doc().get_map("chats").insert_container("chat", LoroMap::new()).unwrap();
            row.insert("id", "chat").unwrap(); row.insert("deviceId", "owner-device").unwrap();
            row.insert("title", "before").unwrap(); remote.doc().commit();
            remote.set_chat_last_message("chat", "before", chrono::DateTime::from_timestamp_millis(1_000).unwrap()).unwrap();
            let raw = LoroDoc::new(); raw.import(&remote.export_snapshot().unwrap()).unwrap();
            let local = WorkspaceDoc::from_doc(raw);
            let binding = local.binding(); journal(&binding);
            local.set_chat_last_message("chat", "local preview", chrono::DateTime::from_timestamp_millis(local_at).unwrap()).unwrap();
            let original = binding.pending_records()[0].clone();
            remote.set_chat_last_message("chat", "remote preview", chrono::DateTime::from_timestamp_millis(remote_at).unwrap()).unwrap();
            remote.rename_chat("chat", "concurrent title").unwrap();
            let snapshot = remote.doc().export(ExportMode::shallow_snapshot(&remote.doc().state_frontiers())).unwrap();
            binding.fail_recovery("retained preview conflict".into());
            assert!(binding.commit().is_err());
            for _ in 0..2 {
                binding.adopt_snapshot(&snapshot, None).unwrap();
                let row = row_value(&binding.raw(), "chats", "chat").unwrap();
                assert_eq!(row["lastMessageAt"], local_at.max(remote_at));
                assert_eq!(row["lastMessagePreview"], if local_at > remote_at { "local preview" } else { "remote preview" });
                assert_eq!(row["title"], "concurrent title");
                assert_eq!(binding.pending_records()[0].before, original.before);
                assert_eq!(binding.pending_records()[0].value, original.value);
            }
            remote.set_chat_last_message("chat", "newest preview", chrono::DateTime::from_timestamp_millis(4_000).unwrap()).unwrap();
            binding.adopt_snapshot(&remote.export_snapshot().unwrap(), None).unwrap();
            let raw = LoroDoc::new(); raw.import(&local.export_snapshot().unwrap()).unwrap();
            let restarted = WorkspaceDoc::from_doc(raw);
            let binding = restarted.binding();
            binding.install_journal(local.binding().pending_records(), Arc::new(|_, _| Ok(())));
            binding.adopt_snapshot(&snapshot, None).unwrap();
            let row = row_value(&binding.raw(), "chats", "chat").unwrap();
            assert_eq!(row["lastMessageAt"], 4_000);
            assert_eq!(row["lastMessagePreview"], "newest preview");
            assert_eq!(binding.pending_records()[0].before, original.before);
            assert_eq!(binding.pending_records()[0].value, original.value);
            let mut created = restarted.chat("chat").unwrap().unwrap();
            created.id = "new-session".into();
            created.last_message_at = None; created.last_message_preview = None;
            restarted.upsert_chat(&created).unwrap();
            let member = comet_proto::SessionRef { chat_id: created.id.clone(), added_at: created.created_at, environment: None, startup: None };
            restarted.upsert_session_ref("owner", &member).unwrap();
            assert_eq!(restarted.chat(&created.id).unwrap(), Some(created.clone()));
            assert_eq!(restarted.session_ref("owner", &created.id).unwrap(), Some(member));
            assert!(restarted.session_ref("other-user", &created.id).unwrap().is_none());

            restarted.rename_chat("chat", "offline rename").unwrap();
            remote.rename_chat("chat", "unobserved competing rename").unwrap();
            remote.set_chat_last_message("chat", "later observation", chrono::DateTime::from_timestamp_millis(5_000).unwrap()).unwrap();
            let preserved = restarted.export_snapshot().unwrap();
            let retained = serde_json::to_vec(&binding.pending_records()).unwrap();
            assert!(binding.adopt_snapshot(&remote.export_snapshot().unwrap(), None).is_err());
            assert_eq!(restarted.export_snapshot().unwrap(), preserved);
            assert_eq!(serde_json::to_vec(&binding.pending_records()).unwrap(), retained);
        }
    }

    #[test]
    fn restarted_session_publications_keep_latest_owner_status_and_retain_conflicting_originals() {
        use comet_proto::{Session, SessionStatus};
        let publication = |at, status| Session {
            chat_id: "chat".into(), device_id: "owner-device".into(), status,
            model_retry: (status == SessionStatus::Working).then_some(comet_proto::ModelRetry { attempt: 2, max_attempts: 4 }),
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
