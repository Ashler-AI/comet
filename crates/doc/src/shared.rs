//! Replaceable Loro cache binding. Durable semantic intents do not depend on Loro ancestry.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use loro::{Diff, EventTriggerKind, ExportMode, Index, ListDiffItem, LoroDoc, LoroList, LoroMap, LoroValue};
use serde::{Deserialize, Serialize};
use crate::{DocError, SessionDoc, SessionCommandEntry, SessionCommandStatus, SessionMessageEntry};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingRecord {
    pub container: String,
    pub key: String,
    pub before: Option<serde_json::Value>,
    pub value: Option<serde_json::Value>,
}
type Persist = Arc<dyn Fn(&[PendingRecord], Option<&[u8]>) -> Result<(), String> + Send + Sync>;
type RootCallback = Arc<dyn for<'a> Fn(loro::DiffEvent<'a>) + Send + Sync>;
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
    pending: Mutex<BTreeMap<(String, String), PendingRecord>>,
    baseline: Mutex<BTreeMap<(String, String), serde_json::Value>>,
    persist: Mutex<Option<Persist>>,
    error: Mutex<Option<String>>,
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
        }))
    }
    pub fn raw(&self) -> LoroDoc { lock(&self.0.state).raw.clone() }
    pub fn operation(&self) -> MutexGuard<'_, ()> { lock(&self.0.gate) }
    pub fn get_map(&self, name: &str) -> LoroMap { self.raw().get_map(name) }
    pub fn get_list(&self, name: &str) -> LoroList { self.raw().get_list(name) }
    pub fn export(&self, mode: ExportMode<'_>) -> Result<Vec<u8>, loro::LoroError> { self.raw().export(mode) }
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
        for record in records { lock(&self.0.pending).insert((record.container.clone(), record.key.clone()), record); }
        *lock(&self.0.persist) = Some(persist);
        let raw = self.raw();
        let baseline = map_rows(&raw);
        *lock(&self.0.baseline) = baseline;
        let subscription = journal_subscription(&self.0, &raw);
        lock(&self.0.state).journal = Some(subscription);
    }
    pub fn pending_records(&self) -> Vec<PendingRecord> { lock(&self.0.pending).values().cloned().collect() }
    /// Persist the reconciled snapshot before switching *all* local bindings under
    /// the mutation gate. Failure leaves the live document and original intents intact.
    pub fn adopt_snapshot(&self, bytes: &[u8], expected_chat: Option<&str>) -> Result<(), DocError> {
        let _operation = self.operation();
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
        let pending = self.pending_records();
        reconcile(&candidate, &pending)?;
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
        *lock(&self.0.baseline) = baseline;
        *lock(&self.0.error) = None;
        Ok(())
    }
}
const MAPS: &[&str] = &["devices", "spaces", "chats", "sessions", "sessionRefs", "worktreeDeletions", "agentSessions"];
fn map_rows(raw: &LoroDoc) -> BTreeMap<(String,String), serde_json::Value> {
    let mut rows = BTreeMap::new();
    for name in MAPS {
        if let serde_json::Value::Object(values) = raw.get_map(name).get_deep_value().to_json_value() {
            for (key, value) in values { rows.insert(((*name).into(), key), value); }
        }
    }
    rows
}
fn journal_subscription(inner: &Arc<Inner>, raw: &LoroDoc) -> loro::Subscription {
    let weak = Arc::downgrade(inner);
    let source = raw.clone();
    raw.subscribe_root(Arc::new(move |event| {
        let Some(inner) = weak.upgrade() else { return };
        let mut changed = BTreeSet::new();
        for diff in &event.events {
            let Some(name) = diff.target.root_name().map(|v| v.to_string()).or_else(|| diff.path.first().and_then(|(id,_)| id.root_name()).map(|v| v.to_string())) else { continue };
            if MAPS.contains(&name.as_str()) {
                if let Some((_, Index::Key(key))) = diff.path.first() { changed.insert((name, key.to_string())); }
                else if let Diff::Map(map) = &diff.diff { for key in map.updated.keys() { changed.insert((name.clone(), key.to_string())); } }
            } else if matches!(name.as_str(), "messages" | "commands" | "publications") {
                if let Some((_, Index::Seq(index))) = diff.path.first() {
                    if let Some(row) = source.get_list(&name).get(*index) {
                        if let Some(key) = row.get_deep_value().to_json_value().get("id").and_then(|v| v.as_str()) { changed.insert((name, key.into())); }
                    }
                } else if let Diff::List(items) = &diff.diff {
                    for item in items { if let ListDiffItem::Insert { insert, .. } = item { for row in insert {
                        if let Some(key) = row.get_deep_value().to_json_value().get("id").and_then(|v| v.as_str()) { changed.insert((name.clone(), key.into())); }
                    } } }
                }
            }
        }
        let mut updates = Vec::new();
        for (container,key) in changed {
            let value = row_value(&source, &container, &key);
            let identity = (container.clone(),key.clone());
            let before = lock(&inner.baseline).get(&identity).cloned();
            if event.triggered_by == EventTriggerKind::Local {
                let mut pending = lock(&inner.pending);
                let record = pending.entry(identity.clone()).or_insert(PendingRecord { container, key, before, value: None });
                record.value = value.clone(); updates.push(record.clone());
            }
            let mut baseline = lock(&inner.baseline);
            if let Some(value) = value { if MAPS.contains(&identity.0.as_str()) { baseline.insert(identity,value); } } else { baseline.remove(&identity); }
        }
        if !updates.is_empty() { if let Some(persist) = lock(&inner.persist).as_ref() {
            if let Err(error) = persist(&updates,None) { *lock(&inner.error) = Some(format!("Crew could not persist accepted records: {error}")); }
        } }
    }))
}
fn row_value(doc: &LoroDoc, container: &str, key: &str) -> Option<serde_json::Value> {
    if MAPS.contains(&container) { return doc.get_map(container).get(key).map(|v| v.get_deep_value().to_json_value()); }
    let list = doc.get_list(container);
    for index in 0..list.len() { if let Some(row) = list.get(index) {
        let map = match &row { loro::ValueOrContainer::Container(loro::Container::Map(map)) => map, _ => continue };
        if map.get("id").map(|v| v.get_deep_value().to_json_value()) == Some(serde_json::Value::String(key.into())) { return Some(row.get_deep_value().to_json_value()); }
    } }
    None
}
fn reconcile(doc: &LoroDoc, pending: &[PendingRecord]) -> Result<(),DocError> {
    let session = SessionDoc::from_doc(doc.clone());
    for record in pending {
        let remote = row_value(doc,&record.container,&record.key);
        if remote == record.value { continue }
        let conflict = || DocError::Schema(format!("Crew recovery conflict in {}/{}; original intent retained",record.container,record.key));
        match record.container.as_str() {
            "messages" => {
                let Some(value) = &record.value else { return Err(conflict()) };
                let local: SessionMessageEntry = serde_json::from_value(value.clone())?;
                if let Some(remote) = remote {
                    let remote: SessionMessageEntry = serde_json::from_value(remote)?;
                    if remote.id != local.id || remote.device_id != local.device_id || remote.role != local.role || remote.created_at != local.created_at || remote.peer_message != local.peer_message { return Err(conflict()) }
                    if remote != local { return Err(conflict()) }
                } else { session.push_message(&local)?; }
            }
            "commands" => {
                let Some(value) = &record.value else { return Err(conflict()) };
                let local: SessionCommandEntry = serde_json::from_value(value.clone())?;
                if let Some(remote) = remote {
                    let remote: SessionCommandEntry = serde_json::from_value(remote)?;
                    if remote.payload != local.payload || remote.issued_by != local.issued_by || remote.issued_at != local.issued_at || remote.expires_at != local.expires_at || remote.based_on != local.based_on { return Err(conflict()) }
                    if remote.status != SessionCommandStatus::Pending { continue }
                    if local.status != SessionCommandStatus::Pending { session.set_command_status(&local.id,local.status,local.resolution.as_deref())?; }
                } else { session.queue_command(&local)?; if local.resolution.is_some() { session.set_command_status(&local.id,local.status,local.resolution.as_deref())?; } }
            }
            "publications" => {
                if remote.is_some() { return Err(conflict()) }
                let record: crate::PublicationRecord = serde_json::from_value(record.value.as_ref().and_then(|v| v.get("record")).cloned().ok_or_else(conflict)?)?;
                session.append_publication(&record)?;
            }
            name if MAPS.contains(&name) => {
                let merged = match (&record.before,&record.value,&remote) {
                    (_,None,None) => continue,
                    (Some(before),None,Some(remote)) if before == remote => None,
                    (None,Some(local),None) => Some(local.clone()),
                    (Some(before),Some(local),Some(remote)) => {
                        let (Some(before),Some(local),Some(remote)) = (before.as_object(),local.as_object(),remote.as_object()) else { return Err(conflict()) };
                        let mut merged = remote.clone();
                        for field in before.keys().chain(local.keys()).collect::<BTreeSet<_>>() {
                            if before.get(field) == local.get(field) { continue }
                            if remote.get(field) != before.get(field) && remote.get(field) != local.get(field) { return Err(conflict()) }
                            if let Some(value) = local.get(field) { merged.insert(field.clone(),value.clone()); } else { merged.remove(field); }
                        }
                        Some(serde_json::Value::Object(merged))
                    }
                    (_,Some(local),Some(remote)) if local == remote => continue,
                    _ => return Err(conflict()),
                };
                let root = doc.get_map(name);
                if let Some(serde_json::Value::Object(fields)) = merged {
                    let row = match root.get(&record.key) { Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) => row, _ => root.insert_container(&record.key,LoroMap::new())? };
                    let old = row.get_deep_value().to_json_value();
                    if let Some(old) = old.as_object() { for field in old.keys() { if !fields.contains_key(field) { row.delete(field)?; } } }
                    for (field,value) in fields { row.insert(&field,LoroValue::from(value))?; }
                } else { root.delete(&record.key)?; }
            }
            _ => return Err(conflict()),
        }
    }
    doc.commit();
    Ok(())
}
