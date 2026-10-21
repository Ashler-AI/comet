use std::collections::{BTreeMap, BTreeSet};

use loro::{LoroDoc, LoroList, LoroMap, LoroText, LoroValue, ToJson};
use serde_json::{Map, Value};
use zeron_proto::{CollaborationPrincipal, PublicationRecord, PublicationValue};

use crate::crew_import::{CrewBackupManifest, CrewImportError};

type Result<T, E = CrewImportError> = std::result::Result<T, E>;

fn unsafe_data(message: impl Into<String>) -> CrewImportError {
    CrewImportError::Unsafe(message.into())
}

// Same grammar as legacy workspace::execution_chat_id, including repeated self aliases.
pub(crate) fn canonical_execution_id(key: &str) -> Option<&str> {
    let (id, suffix) = key.split_once("::session::")?;
    (uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
        && suffix.split("::session::").all(|part| part == id))
    .then_some(id)
}

/// Classify inert bootstrap documents without changing their source snapshots or history.
/// `documents` contains decoded current source snapshots; retained recovery snapshots
/// are also read directly. Associated IDs include manifest, native and source-witness identities.
pub fn inert_bootstrap_snapshots(
    db: &rusqlite::Connection,
    documents: &BTreeMap<String, LoroDoc>,
    associated_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>> {
    use sha2::{Digest, Sha256};

    fn references(value: &Value, parent: &str, physical_source: bool, ids: &mut BTreeSet<String>) {
        let physical_environment = parent == "environment"
            && <zeron_proto::SessionEnvironment as serde::Deserialize>::deserialize(value)
                .is_ok_and(|environment| {
                    matches!(
                        environment.source,
                        zeron_proto::SessionEnvironmentSource::Scaffold { .. }
                    )
                });
        match value {
            Value::String(id) => {
                ids.insert(id.clone());
            }
            Value::Array(values) => {
                for value in values {
                    references(value, parent, false, ids);
                }
            }
            Value::Object(fields) => {
                for (key, value) in fields {
                    // Only a successfully decoded Scaffold environment proves a physical-only pointer.
                    if physical_source && key == "sandbox_id" {
                        continue;
                    }
                    ids.insert(key.clone());
                    references(value, key, physical_environment && key == "source", ids);
                }
            }
            _ => {}
        }
    }

    fn retained_references(doc: &LoroDoc, id: &str, ids: &mut BTreeSet<String>) -> Result<bool> {
        // A trimmed source can hide a prior association even when its current
        // projection is empty. It cannot establish that any orphan is inert.
        if doc.is_shallow() || doc.is_detached() || doc.state_vv() != doc.oplog_vv() {
            return Ok(false);
        }
        let metadata = loro::ContainerID::new_root("meta", loro::ContainerType::Map);
        for (peer, end) in doc.oplog_vv().iter() {
            // Enumerate child-container operations too; inspecting a parent
            // container ID or its latest value cannot recover deleted fields.
            for change in doc.export_json_in_id_span(loro::IdSpan::new(*peer, 0, *end)) {
                for op in change.ops {
                    match op.content {
                        loro::JsonOpContent::Map(loro::JsonMapOp::Insert { key, value }) => {
                            let value = value.to_json_value();
                            if op.container == metadata
                                && key == "chatId"
                                && value.as_str() == Some(id)
                            {
                                continue;
                            }
                            ids.insert(key.clone());
                            // The same typed physical-resource exception applies
                            // to retained whole environment values, not a bare
                            // sandbox_id field with unknown historical ancestry.
                            references(&value, &key, false, ids);
                        }
                        loro::JsonOpContent::Map(loro::JsonMapOp::Delete { key }) => {
                            ids.insert(key);
                        }
                        content => references(&serde_json::to_value(content)?, "", false, ids),
                    }
                }
            }
        }
        Ok(true)
    }

    let mut referenced = associated_ids.clone();
    for (id, doc) in documents {
        if !retained_references(doc, id, &mut referenced)? {
            return Ok(BTreeMap::new());
        }
    }
    let roots: BTreeMap<_, _> = documents
        .iter()
        .map(|(id, doc)| (id.as_str(), doc.get_deep_value().to_json_value()))
        .collect();
    for (id, root) in &roots {
        for (key, value) in root
            .as_object()
            .ok_or_else(|| unsafe_data("invalid source document root"))?
        {
            referenced.insert(key.clone());
            if key == "meta" {
                let meta = value
                    .as_object()
                    .ok_or_else(|| unsafe_data("invalid source document metadata"))?;
                for (field, value) in meta {
                    if field == "chatId" && value.as_str() == Some(*id) {
                        continue;
                    }
                    referenced.insert(field.clone());
                    references(value, field, false, &mut referenced);
                }
            } else {
                references(value, key, false, &mut referenced);
            }
        }
    }
    let mut recovery =
        db.prepare("SELECT doc_id,bytes FROM snapshots WHERE substr(doc_id,1,length(?1))=?1")?;
    let mut rows = recovery.query(["recovery-original/"])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        if let Some(original) = id.strip_prefix("recovery-original/") {
            referenced.insert(original.to_owned());
            referenced.extend(original.split('/').map(str::to_owned));
        }
        let bytes: Vec<u8> = row.get(1)?;
        let doc = LoroDoc::new();
        let Ok(status) = doc.import(&bytes) else {
            return Ok(BTreeMap::new());
        };
        if status.pending.is_some() || !retained_references(&doc, &id, &mut referenced)? {
            return Ok(BTreeMap::new());
        }
        references(
            &doc.get_deep_value().to_json_value(),
            "",
            false,
            &mut referenced,
        );
    }
    for (table, identity, payload) in [
        ("document_scopes", "doc_id", Some("scope")),
        ("semantic_intents", "doc_id", Some("value")),
        ("crew_directory_jobs", "session_id", None),
        ("pending_nudges", "chat_id", None),
        ("session_startups", "chat_id", None),
    ] {
        if !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )? {
            continue;
        }
        // Fixed schema identifiers only. Missing/wrong columns or payload types fail closed.
        let sql = format!(
            "SELECT {identity}{} FROM {table}",
            payload.map_or(String::new(), |field| format!(",{field}"))
        );
        let mut statement = db.prepare(&sql)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            referenced.insert(row.get::<_, String>(0)?);
            if payload.is_some() {
                let bytes = match row.get_ref(1)? {
                    rusqlite::types::ValueRef::Text(bytes)
                    | rusqlite::types::ValueRef::Blob(bytes) => bytes,
                    _ => return Err(unsafe_data("invalid bootstrap association payload type")),
                };
                let value: Value = serde_json::from_slice(bytes)?;
                if !value.is_object() {
                    return Err(unsafe_data("invalid bootstrap association payload"));
                }
                references(&value, "", false, &mut referenced);
            }
        }
    }
    let mut eligible = BTreeMap::new();
    for (id, doc) in documents {
        if id == "workspace3"
            || id.starts_with("recovery-original/")
            || referenced.contains(id)
            || doc.is_shallow()
            || doc.is_detached()
            || doc.state_vv() != doc.oplog_vv()
        {
            continue;
        }
        let root = roots[id.as_str()]
            .as_object()
            .ok_or_else(|| unsafe_data("invalid bootstrap root"))?;
        if root.iter().any(|(key, value)| match key.as_str() {
            "meta" => false,
            "messages" | "commands" => !value.as_array().is_some_and(Vec::is_empty),
            _ => true,
        }) {
            continue;
        }
        let Some(meta) = root.get("meta").and_then(Value::as_object) else {
            continue;
        };
        if meta.get("chatId").and_then(Value::as_str) != Some(id.as_str())
            || meta.get("schemaVersion").and_then(Value::as_u64) != Some(2)
            || meta.iter().any(|(key, value)| match key.as_str() {
                "chatId" | "schemaVersion" => false,
                "directoryCompletedTurn" => value.as_str() != Some(""),
                _ => true,
            })
        {
            continue;
        }
        let version = doc.oplog_vv();
        if version.iter().map(|(_, end)| i64::from(*end)).sum::<i64>() != meta.len() as i64 {
            continue;
        }
        let mut inserted = BTreeSet::new();
        // Unlike export_json_updates, ID-span export never implicitly commits the document.
        let only_initial_metadata = version.iter().all(|(peer, end)| {
            doc.export_json_in_id_span(loro::IdSpan::new(*peer, 0, *end))
                .into_iter()
                .flat_map(|change| change.ops)
                .all(|op| {
                    if op.container != loro::ContainerID::new_root("meta", loro::ContainerType::Map)
                    {
                        return false;
                    }
                    let loro::JsonOpContent::Map(loro::JsonMapOp::Insert { key, value }) =
                        op.content
                    else {
                        return false;
                    };
                    meta.get(&key) == Some(&value.to_json_value()) && inserted.insert(key)
                })
        });
        if !only_initial_metadata || inserted.len() != meta.len() {
            continue;
        }
        let bytes: Vec<u8> =
            db.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [id], |row| {
                row.get(0)
            })?;
        eligible.insert(id.clone(), format!("{:x}", Sha256::digest(&bytes)));
    }
    Ok(eligible)
}

/// Reconcile offline semantic records, never CRDT-import two independently initialized
/// rooms. Their map/list operations have unrelated identity and could hide records.
/// Originals remain in the inventoried SQLite backup. No command is executed here.
/// Validate the entire plan before replacing any caller-owned document.
pub(crate) fn reconcile_execution_snapshots(
    documents: &mut BTreeMap<String, LoroDoc>,
    manifest: &CrewBackupManifest,
    principal: &CollaborationPrincipal,
) -> Result<BTreeMap<String, String>> {
    let mut aliases = BTreeMap::new();
    for key in documents.keys().filter(|key| key.contains("::session::")) {
        let canonical = canonical_execution_id(key).ok_or_else(|| {
            unsafe_data(format!(
                "distinct or invalid private execution identity: {key}"
            ))
        })?;
        aliases.insert(key.clone(), canonical.to_owned());
    }
    if principal.session_id.is_some()
        || principal.subject != manifest.owner_subject
        || principal.project_id != manifest.project_id
        || principal.deployment_id != manifest.deployment_id
    {
        return Err(unsafe_data(
            "private execution import requires verified manifest owner scope",
        ));
    }
    let workspace_doc = documents
        .get("workspace3")
        .ok_or_else(|| unsafe_data("private execution has no workspace owner binding"))?;
    let mut workspace = workspace_doc.get_deep_value().to_json_value();
    normalize_workspace(
        workspace_doc,
        &mut workspace,
        &mut aliases,
        manifest,
        principal,
    )?;
    if aliases.is_empty() {
        return Ok(aliases);
    }
    complete(workspace_doc)?;
    let mut candidates = BTreeMap::<String, Value>::new();
    for (alias, canonical) in &aliases {
        let binding = manifest.chats.get(canonical).ok_or_else(|| {
            unsafe_data(format!(
                "private execution has no canonical manifest binding: {alias}"
            ))
        })?;
        binding.validate().map_err(unsafe_data)?;
        if binding.session_id != *canonical
            || binding.owner_subject != principal.subject
            || binding.scope.project_id != principal.project_id
            || binding.scope.deployment_id != principal.deployment_id
        {
            return Err(unsafe_data(format!(
                "private execution canonical owner/scope mismatch: {alias}"
            )));
        }
        let owner_device = workspace
            .get("chats")
            .and_then(|rows| rows.get(canonical))
            .and_then(|row| row.get("deviceId"))
            .and_then(Value::as_str)
            .filter(|device| !device.is_empty())
            .ok_or_else(|| {
                unsafe_data(format!(
                    "private execution has no canonical workspace device: {alias}"
                ))
            })?;
        if !candidates.contains_key(canonical) {
            let target = documents.get(canonical).ok_or_else(|| {
                unsafe_data(format!(
                    "private execution has no canonical snapshot: {alias}"
                ))
            })?;
            complete(target)?;
            let mut root = target.get_deep_value().to_json_value();
            validate_source(&mut root, canonical, canonical, principal, owner_device)?;
            candidates.insert(canonical.clone(), root);
        }
        let Some(source) = documents.get(alias) else {
            continue;
        };
        complete(source)?;
        let mut source = source.get_deep_value().to_json_value();
        validate_source(&mut source, alias, canonical, principal, owner_device)?;
        merge_root(
            candidates.get_mut(canonical).expect("candidate inserted"),
            source,
        )?;
    }
    let mut replacements = BTreeMap::new();
    for (id, root) in candidates {
        replacements.insert(id, materialize(&root)?);
    }
    replacements.insert("workspace3".into(), materialize_workspace(&workspace)?);
    for alias in aliases.keys() {
        documents.remove(alias);
    }
    documents.extend(replacements);
    Ok(aliases)
}

fn complete(doc: &LoroDoc) -> Result<()> {
    if doc.is_detached() || doc.state_vv() != doc.oplog_vv() {
        return Err(unsafe_data("incomplete private execution snapshot"));
    }
    Ok(())
}

fn normalize_id(value: &mut Value, canonical: &str) -> Result<()> {
    let id = value
        .as_str()
        .ok_or_else(|| unsafe_data("execution identity is not a string"))?;
    if id != canonical && canonical_execution_id(id) != Some(canonical) {
        return Err(unsafe_data(format!(
            "shared or foreign execution membership: {id}"
        )));
    }
    *value = canonical.into();
    Ok(())
}

fn validate_source(
    root: &mut Value,
    alias: &str,
    canonical: &str,
    principal: &CollaborationPrincipal,
    device: &str,
) -> Result<()> {
    let meta = root
        .get_mut("meta")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| unsafe_data("execution snapshot has no metadata"))?;
    if !matches!(
        meta.get("schemaVersion").and_then(Value::as_u64),
        Some(1 | 2)
    ) {
        return Err(unsafe_data("unsupported private execution schema"));
    }
    let id = meta
        .get_mut("chatId")
        .ok_or_else(|| unsafe_data("execution snapshot has no chatId"))?;
    // A filename is not authority: validate the original identity before rewriting.
    if id.as_str() != Some(alias) && id.as_str() != Some(canonical) {
        return Err(unsafe_data(
            "execution snapshot has a different logical owner",
        ));
    }
    *id = canonical.into();
    if let Some(messages) = root.get_mut("messages") {
        for message in messages
            .as_array_mut()
            .ok_or_else(|| unsafe_data("invalid execution messages"))?
        {
            if message.get("role").and_then(Value::as_str) == Some("assistant")
                && message.get("deviceId").and_then(Value::as_str) != Some(device)
            {
                return Err(unsafe_data("private output belongs to another device"));
            }
            if let Some(peer) = message
                .get_mut("peerMessage")
                .and_then(|peer| peer.get_mut("sourceChatId"))
            {
                if peer
                    .as_str()
                    .is_some_and(|id| canonical_execution_id(id) == Some(canonical))
                {
                    *peer = canonical.into();
                } else if peer.as_str().is_some_and(|id| id.contains("::session::")) {
                    return Err(unsafe_data("unresolved private peer provenance"));
                }
            }
        }
    }
    if let Some(publications) = root.get_mut("publications") {
        for row in publications
            .as_array_mut()
            .ok_or_else(|| unsafe_data("invalid execution publications"))?
        {
            let old_id = row
                .get("id")
                .cloned()
                .ok_or_else(|| unsafe_data("publication without identity"))?;
            let record = row
                .get_mut("record")
                .ok_or_else(|| unsafe_data("publication without record"))?;
            if record.get("id") != Some(&old_id) {
                return Err(unsafe_data("publication identity mismatch"));
            }
            normalize_publication(record, alias, canonical, principal, device)?;
            let normalized_id = record["id"].clone();
            row["id"] = normalized_id;
        }
    }
    if let Some(register) = root.get_mut("agentSessions") {
        let old = std::mem::take(
            register
                .as_object_mut()
                .ok_or_else(|| unsafe_data("invalid agent session register"))?,
        );
        for (key, mut record) in old {
            let mut normalized = Value::String(key);
            normalize_id(&mut normalized, canonical)?;
            if record.get("kind").and_then(Value::as_str) != Some("agentSession") {
                return Err(unsafe_data("non-session private owner register"));
            }
            normalize_publication(&mut record, alias, canonical, principal, device)?;
            let rows = register.as_object_mut().expect("validated register");
            if let Some(previous) = rows.insert(canonical.into(), record.clone()) {
                if previous != record {
                    return Err(unsafe_data("conflicting private owner registers"));
                }
            }
        }
    }
    Ok(())
}

fn normalize_publication(
    record: &mut Value,
    alias: &str,
    canonical: &str,
    principal: &CollaborationPrincipal,
    device: &str,
) -> Result<()> {
    let owner = principal.subject.as_str();
    let publication: PublicationRecord = serde_json::from_value(record.clone())?;
    if publication.published_by != owner {
        return Err(unsafe_data("foreign private publication owner"));
    }
    match publication.value {
        PublicationValue::MessageProvenance(provenance) => {
            if provenance.author_subject != owner || provenance.owner_device_id != device {
                return Err(unsafe_data("foreign private message provenance"));
            }
            normalize_id(&mut record["value"]["sessionId"], canonical)?;
            let normalized = if canonical_execution_id(&provenance.session_id).is_some() {
                publication.id.replace(&provenance.session_id, canonical)
            } else {
                publication.id
            };
            record["id"] = normalized.replace(alias, canonical).into();
        }
        PublicationValue::AgentSession(session) => {
            if session.owner_subject != owner || session.owner_device_id != device {
                return Err(unsafe_data("foreign private agent membership"));
            }
            normalize_id(&mut record["value"]["sessionId"], canonical)?;
            normalize_id(&mut record["value"]["chatId"], canonical)?;
            // Environment grants are not alias-rewritable authority.
            if session.environment.as_ref().is_some_and(|environment| {
                environment.owner_principal != owner
                    || environment.scope.session_id.as_deref() != Some(canonical)
                    || environment.scope.project_id != principal.project_id
                    || environment.scope.deployment_id != principal.deployment_id
            }) {
                return Err(unsafe_data(
                    "private environment requires authenticated reattachment",
                ));
            }
        }
        PublicationValue::Unknown { .. } => {
            return Err(unsafe_data("unknown private publication kind"));
        }
        _ => {}
    }
    let normalized: PublicationRecord = serde_json::from_value(record.clone())?;
    zeron_doc::validate_publication(&normalized)?;
    Ok(())
}

fn merge_root(target: &mut Value, source: Value) -> Result<()> {
    let target = target
        .as_object_mut()
        .ok_or_else(|| unsafe_data("invalid canonical document root"))?;
    let applied: BTreeSet<String> = target
        .get("commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|row| row.get("status").and_then(Value::as_str) == Some("applied"))
        .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    for (container, value) in source
        .as_object()
        .ok_or_else(|| unsafe_data("invalid execution document root"))?
    {
        match container.as_str() {
            "messages" | "commands" | "publications" => {
                let rows = target
                    .entry(container.clone())
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .ok_or_else(|| unsafe_data("invalid canonical list"))?;
                let mut ids = BTreeMap::new();
                for (index, row) in rows.iter().enumerate() {
                    if ids.insert(record_id(row)?.to_owned(), index).is_some() {
                        return Err(unsafe_data("duplicate canonical record identity"));
                    }
                }
                let mut source_ids = BTreeSet::new();
                for row in value
                    .as_array()
                    .ok_or_else(|| unsafe_data("invalid execution list"))?
                {
                    let id = record_id(row)?;
                    if !source_ids.insert(id) {
                        return Err(unsafe_data("duplicate execution record identity"));
                    }
                    if let Some(index) = ids.get(id).copied() {
                        let current = &mut rows[index];
                        if current == row {
                            continue;
                        }
                        match container.as_str() {
                            "messages" => merge_message(current, row, applied.contains(id))?,
                            "commands" => merge_command(current, row)?,
                            _ => {
                                return Err(unsafe_data(format!(
                                    "conflicting immutable publication: {id}"
                                )));
                            }
                        }
                    } else {
                        ids.insert(id.to_owned(), rows.len());
                        rows.push(row.clone());
                    }
                }
            }
            "meta" | "agentSessions" | "crewAnnotations" => {
                let fields = target
                    .entry(container.clone())
                    .or_insert_with(|| Value::Object(Map::new()))
                    .as_object_mut()
                    .ok_or_else(|| unsafe_data("invalid canonical map"))?;
                for (key, value) in value
                    .as_object()
                    .ok_or_else(|| unsafe_data("invalid execution map"))?
                {
                    match fields.get(key) {
                        Some(current) if current == value => {}
                        Some(current) if container == "meta" && key == "schemaVersion" => {
                            fields.insert(
                                key.clone(),
                                Value::from(
                                    current
                                        .as_u64()
                                        .unwrap_or(0)
                                        .max(value.as_u64().unwrap_or(0)),
                                ),
                            );
                        }
                        Some(_) => {
                            return Err(unsafe_data(format!(
                                "conflicting execution field: {container}/{key}"
                            )));
                        }
                        None => {
                            fields.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
            _ if target.get(container) == Some(value) => {}
            _ => {
                return Err(unsafe_data(format!(
                    "unknown execution container: {container}"
                )));
            }
        }
    }
    Ok(())
}

fn record_id(row: &Value) -> Result<&str> {
    row.get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| unsafe_data("execution record without identity"))
}

fn equal_except(a: &Value, b: &Value, excluded: &[&str]) -> bool {
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else {
        return false;
    };
    a.iter()
        .filter(|(key, _)| !excluded.contains(&key.as_str()))
        .all(|(key, value)| b.get(key) == Some(value))
        && b.keys()
            .filter(|key| !excluded.contains(&key.as_str()))
            .all(|key| a.contains_key(key))
}

fn merge_command(current: &mut Value, incoming: &Value) -> Result<()> {
    let terminal = |value: &Value| {
        matches!(
            value.get("status").and_then(Value::as_str),
            Some("applied" | "rejected" | "expired" | "superseded" | "cancelled")
        )
    };
    if equal_except(current, incoming, &["status", "resolution"]) {
        if current["status"] == "pending"
            && current.get("resolution").is_none_or(Value::is_null)
            && terminal(incoming)
        {
            *current = incoming.clone();
            return Ok(());
        }
        if incoming["status"] == "pending"
            && incoming.get("resolution").is_none_or(Value::is_null)
            && terminal(current)
        {
            return Ok(());
        }
    }
    Err(unsafe_data("conflicting private durable command outcome"))
}

fn merge_message(current: &mut Value, incoming: &Value, applied: bool) -> Result<()> {
    let role = current.get("role").and_then(Value::as_str);
    if role == Some("user")
        && incoming["role"] == "user"
        && current["parts"] == incoming["parts"]
        && applied
        && equal_except(current, incoming, &["createdAt", "status"])
    {
        return Ok(()); // Legacy accepted user command owns its authoritative clock.
    }
    if !equal_except(current, incoming, &["parts", "status"]) {
        return Err(unsafe_data("conflicting private message identity"));
    }
    let before = current.get("status").and_then(Value::as_str);
    let after = incoming.get("status").and_then(Value::as_str);
    if role != Some("assistant") && current["parts"] == incoming["parts"] {
        match (before, after) {
            (Some("complete" | "aborted"), Some("queued" | "steered") | None)
            | (Some("steered"), Some("queued") | None) => return Ok(()),
            (Some("queued" | "steered") | None, Some("complete" | "aborted" | "steered")) => {
                current["status"] = incoming["status"].clone();
                return Ok(());
            }
            _ => {}
        }
    }
    if role == Some("assistant") {
        let old = current["parts"]
            .as_array()
            .ok_or_else(|| unsafe_data("invalid message parts"))?;
        let new = incoming["parts"]
            .as_array()
            .ok_or_else(|| unsafe_data("invalid message parts"))?;
        if matches!(before, Some("complete" | "aborted"))
            && after == Some("streaming")
            && parts_prefix(new, old)
        {
            return Ok(());
        }
        if before == Some("streaming") {
            if after == Some("streaming") && parts_prefix(new, old) {
                return Ok(());
            }
            if matches!(after, Some("streaming" | "complete" | "aborted")) && parts_prefix(old, new)
            {
                *current = incoming.clone();
                return Ok(());
            }
            if after == Some("aborted")
                && new.last().is_some_and(|part| part["kind"] == "error")
                && parts_prefix(&new[..new.len() - 1], old)
            {
                let error = new.last().expect("checked error");
                if let Some(existing) = old.iter().find(|part| part.get("id") == error.get("id")) {
                    if existing != error {
                        return Err(unsafe_data("conflicting recovery interruption"));
                    }
                } else {
                    current["parts"]
                        .as_array_mut()
                        .expect("validated parts")
                        .push(error.clone());
                }
                current["status"] = "aborted".into();
                return Ok(());
            }
        }
    }
    // Unlike live legacy recovery, never drop richer streaming content in favor
    // of an older terminal snapshot. Offline ambiguity requires reconciliation.
    Err(unsafe_data("conflicting private transcript content"))
}

fn parts_prefix(old: &[Value], new: &[Value]) -> bool {
    old.len() <= new.len()
        && old.iter().zip(new).all(|(old, new)| {
            if old == new {
                return true;
            }
            if old.get("id") != new.get("id") || old.get("kind") != new.get("kind") {
                return false;
            }
            match old.get("kind").and_then(Value::as_str) {
                Some("text") => {
                    equal_except(old, new, &["text"])
                        && old["text"]
                            .as_str()
                            .zip(new["text"].as_str())
                            .is_some_and(|(old, new)| new.starts_with(old))
                }
                Some("input") => {
                    equal_except(old, new, &["resolved"])
                        && (old["resolved"] == false || new["resolved"] == true)
                }
                Some("thinking") => {
                    equal_except(old, new, &["started", "completed"])
                        && ["started", "completed"]
                            .iter()
                            .all(|key| old[*key] == false || new[*key] == true)
                }
                Some("tool") => {
                    equal_except(old, new, &["call", "isError", "resolved"])
                        && (old["resolved"] == false
                            || (new["resolved"] == true
                                && old["isError"] == new["isError"]
                                && tool_call_prefix(&old["call"], &new["call"])))
                }
                _ => false,
            }
        })
}

fn tool_call_prefix(old: &Value, new: &Value) -> bool {
    if old == new {
        return true;
    }
    if old["kind"] != "todo" || new["kind"] != "todo" || !equal_except(old, new, &["items"]) {
        return false;
    }
    let (Some(old), Some(new)) = (old["items"].as_array(), new["items"].as_array()) else {
        return false;
    };
    old.len() <= new.len()
        && old.iter().zip(new).all(|(old, new)| {
            equal_except(old, new, &["done"]) && (old["done"] == false || new["done"] == true)
        })
}

fn materialize(root: &Value) -> Result<LoroDoc> {
    let doc = LoroDoc::new();
    for (key, value) in root
        .as_object()
        .ok_or_else(|| unsafe_data("invalid reconciled root"))?
    {
        match value {
            Value::Object(fields) => write_map(&doc.get_map(key.as_str()), fields)?,
            Value::Array(rows) => write_list(&doc.get_list(key.as_str()), rows)?,
            _ => return Err(unsafe_data("unsupported reconciled root type")),
        }
    }
    doc.commit();
    Ok(doc)
}

fn write_map(map: &LoroMap, fields: &Map<String, Value>) -> Result<()> {
    for (key, value) in fields {
        match value {
            Value::Array(rows) if key == "parts" => {
                write_list(&map.insert_container(key, LoroList::new())?, rows)?
            }
            Value::String(text) if matches!(key.as_str(), "text" | "reasoning") => {
                map.insert_container(key, LoroText::new())?
                    .insert(0, text)?;
            }
            _ => {
                map.insert(key, LoroValue::from(value.clone()))?;
            }
        }
    }
    Ok(())
}

fn write_list(list: &LoroList, rows: &[Value]) -> Result<()> {
    for row in rows {
        let fields = row
            .as_object()
            .ok_or_else(|| unsafe_data("non-object semantic list row"))?;
        write_map(&list.push_container(LoroMap::new())?, fields)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inert_bootstrap_rejects_deleted_cross_document_references_and_unknown_history() {
        use loro::ExportMode;
        let id = "generic-historically-referenced-bootstrap";
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE snapshots(doc_id TEXT PRIMARY KEY, bytes BLOB NOT NULL);")
            .unwrap();
        let candidate = LoroDoc::new();
        candidate.get_map("meta").insert("chatId", id).unwrap();
        candidate
            .get_map("meta")
            .insert("schemaVersion", 2)
            .unwrap();
        candidate.commit();
        let bytes = candidate.export(ExportMode::Snapshot).unwrap();
        db.execute(
            "INSERT INTO snapshots VALUES (?1,?2)",
            rusqlite::params![id, &bytes],
        )
        .unwrap();
        let mut documents = BTreeMap::from([(id.into(), candidate)]);
        let mut incorrectly_classified = Vec::new();
        for scenario in [
            "deleted-reference",
            "deleted-nested-reference",
            "deleted-reference-key",
            "shallow-unknown-history",
        ] {
            let workspace = LoroDoc::new();
            let key = if scenario == "deleted-reference-key" {
                id
            } else {
                "prior-membership"
            };
            if scenario == "deleted-nested-reference" {
                let child = workspace
                    .get_map("sessionRefs")
                    .insert_container(key, LoroMap::new())
                    .unwrap();
                child.insert("chatId", id).unwrap();
                workspace.commit();
                child.delete("chatId").unwrap();
            } else {
                let value = if scenario == "deleted-reference-key" {
                    json!({"chatId":"another-chat"})
                } else {
                    json!({"chatId":id})
                };
                workspace
                    .get_map("sessionRefs")
                    .insert(key, LoroValue::from(value))
                    .unwrap();
                workspace.commit();
                workspace.get_map("sessionRefs").delete(key).unwrap();
            }
            workspace.commit();
            let workspace = if scenario == "shallow-unknown-history" {
                let shallow = LoroDoc::new();
                shallow
                    .import(
                        &workspace
                            .export(ExportMode::shallow_snapshot(&workspace.oplog_frontiers()))
                            .unwrap(),
                    )
                    .unwrap();
                assert!(shallow.is_shallow());
                shallow
            } else {
                workspace
            };
            documents.insert("workspace3".into(), workspace);
            if inert_bootstrap_snapshots(&db, &documents, &BTreeSet::new())
                .unwrap()
                .contains_key(id)
            {
                incorrectly_classified.push(scenario);
            }
        }
        assert!(
            incorrectly_classified.is_empty(),
            "historical association or unknown history was classified inert: {incorrectly_classified:?}"
        );
        assert_eq!(
            db.query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [id], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .unwrap(),
            bytes
        );
    }

    #[test]
    fn inert_bootstrap_requires_pristine_history_and_no_semantic_association() {
        use loro::ExportMode;
        use sha2::{Digest, Sha256};
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE snapshots(doc_id TEXT PRIMARY KEY, bytes BLOB NOT NULL);")
            .unwrap();
        let id = "generic-orphan";
        let make = || {
            let doc = LoroDoc::new();
            doc.get_map("meta").insert("chatId", id).unwrap();
            doc.get_map("meta").insert("schemaVersion", 2).unwrap();
            doc.get_map("meta")
                .insert("directoryCompletedTurn", "")
                .unwrap();
            doc.commit();
            doc
        };
        let doc = make();
        let bytes = doc.export(ExportMode::Snapshot).unwrap();
        db.execute(
            "INSERT INTO snapshots VALUES (?1,?2)",
            rusqlite::params![id, &bytes],
        )
        .unwrap();
        let mut docs = BTreeMap::from([(id.to_owned(), doc)]);
        let expected = BTreeMap::from([(id.to_owned(), format!("{:x}", Sha256::digest(&bytes)))]);
        assert_eq!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new()).unwrap(),
            expected
        );
        assert!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::from([id.into()]))
                .unwrap()
                .is_empty()
        );
        for scenario in [
            "content",
            "deleted-content",
            "metadata-history",
            "shallow",
            "extra-root",
        ] {
            let doc = make();
            match scenario {
                "content" => {
                    doc.get_list("messages").push("kept").unwrap();
                }
                "deleted-content" => {
                    doc.get_list("commands").push("old").unwrap();
                    doc.commit();
                    doc.get_list("commands").delete(0, 1).unwrap();
                }
                "metadata-history" => {
                    doc.get_map("meta")
                        .insert("directoryCompletedTurn", "old")
                        .unwrap();
                    doc.commit();
                    doc.get_map("meta")
                        .insert("directoryCompletedTurn", "")
                        .unwrap();
                }
                "extra-root" => {
                    doc.get_map("crewAnnotations")
                        .insert("old", "annotation")
                        .unwrap();
                    doc.get_map("crewAnnotations").delete("old").unwrap();
                }
                _ => {}
            }
            doc.commit();
            let doc = if scenario == "shallow" {
                let shallow = LoroDoc::new();
                shallow
                    .import(
                        &doc.export(ExportMode::shallow_snapshot(&doc.oplog_frontiers()))
                            .unwrap(),
                    )
                    .unwrap();
                shallow
            } else {
                doc
            };
            docs.insert(id.into(), doc);
            assert!(
                inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new())
                    .unwrap()
                    .is_empty(),
                "{scenario}"
            );
        }
        docs.insert(id.into(), make());
        let workspace = LoroDoc::new();
        workspace
            .get_map("sessionRefs")
            .insert("other", LoroValue::from(json!({"chatId":id})))
            .unwrap();
        workspace.commit();
        docs.insert("workspace3".into(), workspace);
        assert!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
        docs.remove("workspace3");
        let environment = json!({"source":{"kind":"scaffold","sandbox_id":id,"lifecycle":"stopped"},
            "ownerPrincipal":"owner","scope":{"projectId":"project","deploymentId":"deployment","sessionId":"actual-chat"}});
        for scenario in [
            "typed-physical",
            "deleted-typed-physical",
            "logical-scope",
            "untyped-physical",
        ] {
            let mut environment = environment.clone();
            if scenario == "logical-scope" {
                environment["scope"]["sessionId"] = json!(id);
            }
            if scenario == "untyped-physical" {
                environment
                    .as_object_mut()
                    .unwrap()
                    .remove("ownerPrincipal");
            }
            let workspace = LoroDoc::new();
            workspace
                .get_map("sessionRefs")
                .insert(
                    "actual-chat",
                    LoroValue::from(json!({"chatId":"actual-chat","environment":environment})),
                )
                .unwrap();
            workspace.commit();
            if scenario == "deleted-typed-physical" {
                workspace
                    .get_map("sessionRefs")
                    .delete("actual-chat")
                    .unwrap();
                workspace.commit();
            }
            docs.insert("workspace3".into(), workspace);
            let classified = inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new()).unwrap();
            assert_eq!(
                classified.contains_key(id),
                matches!(scenario, "typed-physical" | "deleted-typed-physical"),
                "{scenario}"
            );
        }
        docs.remove("workspace3");
        let recovered = make();
        db.execute(
            "INSERT INTO snapshots VALUES ('recovery-original/prior-scope',?1)",
            [recovered.export(ExportMode::Snapshot).unwrap()],
        )
        .unwrap();
        assert!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
        db.execute(
            "UPDATE snapshots SET bytes=?1 WHERE doc_id='recovery-original/prior-scope'",
            [b"opaque retained bytes".as_slice()],
        )
        .unwrap();
        assert!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
        db.execute(
            "DELETE FROM snapshots WHERE doc_id='recovery-original/prior-scope'",
            [],
        )
        .unwrap();
        for (table, identity, payload) in [
            ("document_scopes", "doc_id", Some("scope")),
            ("semantic_intents", "doc_id", Some("value")),
            ("crew_directory_jobs", "session_id", None),
            ("pending_nudges", "chat_id", None),
            ("session_startups", "chat_id", None),
        ] {
            db.execute_batch(&format!(
                "CREATE TABLE {table}({identity} TEXT{});",
                payload.map_or(String::new(), |field| format!(",{field} TEXT"))
            ))
            .unwrap();
            if payload.is_some() {
                db.execute(
                    &format!("INSERT INTO {table} VALUES ('other',?1)"),
                    [json!({"session":id}).to_string()],
                )
                .unwrap();
            } else {
                db.execute(&format!("INSERT INTO {table} VALUES (?1)"), [id])
                    .unwrap();
            }
            assert!(
                inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new())
                    .unwrap()
                    .is_empty(),
                "{table}"
            );
            db.execute_batch(&format!("DROP TABLE {table}")).unwrap();
        }
        let retained: Vec<u8> = db
            .query_row("SELECT bytes FROM snapshots WHERE doc_id=?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(retained, bytes);
        assert_eq!(
            inert_bootstrap_snapshots(&db, &docs, &BTreeSet::new()).unwrap(),
            expected
        );
    }

    const CHAT: &str = "00000000-0000-4000-8000-000000000213";

    fn fixture() -> (
        BTreeMap<String, LoroDoc>,
        CrewBackupManifest,
        CollaborationPrincipal,
        String,
    ) {
        let alias = format!("{CHAT}::session::{CHAT}::session::{CHAT}");
        let manifest = serde_json::from_value(json!({
            "version":1,"writerStopped":true,"sourceProfileRoot":"/legacy","ownerSubject":"owner",
            "projectId":"project","deploymentId":null,"files":{},"chats":{
                (CHAT):{"ownerSubject":"owner","sessionId":CHAT,"scope":{"projectId":"project","sessionId":CHAT}}
            }
        })).unwrap();
        let principal =
            serde_json::from_value(json!({"subject":"owner","projectId":"project"})).unwrap();
        let root = json!({"meta":{"chatId":CHAT,"schemaVersion":2},"messages":[],"commands":[],"publications":[]});
        let mut documents = BTreeMap::new();
        let workspace = LoroDoc::new();
        workspace
            .get_map("chats")
            .insert(
                CHAT,
                LoroValue::from(json!({"id":CHAT,"deviceId":"device"})),
            )
            .unwrap();
        workspace.commit();
        documents.insert("workspace3".into(), workspace);
        documents.insert(CHAT.into(), materialize(&root).unwrap());
        let mut private = root;
        private["meta"]["chatId"] = alias.clone().into();
        documents.insert(alias.clone(), materialize(&private).unwrap());
        (documents, manifest, principal, alias)
    }

    #[test]
    fn preserves_private_records_and_native_associations() {
        let (mut documents, manifest, principal, alias) = fixture();
        let mut private = documents[&alias].get_deep_value().to_json_value();
        private["messages"] = json!([{"id":"answer","role":"assistant","deviceId":"device","createdAt":1,"status":"complete",
            "parts":[{"id":"text","kind":"text","text":"private output"},{"id":"think","kind":"thinking","started":true,"completed":true}]}]);
        private["commands"] =
            json!([{"id":"accepted","status":"applied","payload":{"kind":"interrupt"}}]);
        private["crewAnnotations"] =
            json!({"note":{"id":"note","body":"keep","anchor":{"messageId":"answer"}}});
        private["meta"]["harnessSessionId"] = "native-history".into();
        documents.insert(alias.clone(), materialize(&private).unwrap());
        assert_eq!(
            reconcile_execution_snapshots(&mut documents, &manifest, &principal).unwrap(),
            BTreeMap::from([(alias.clone(), CHAT.into())])
        );
        assert!(!documents.contains_key(&alias));
        let result = documents[CHAT].get_deep_value().to_json_value();
        assert_eq!(result["messages"], private["messages"]);
        assert_eq!(result["commands"], private["commands"]);
        assert_eq!(result["crewAnnotations"], private["crewAnnotations"]);
        assert_eq!(result["meta"]["harnessSessionId"], "native-history");
        assert_eq!(result["meta"]["chatId"], CHAT);
    }

    #[test]
    fn foreign_ownership_and_distinct_children_fail_without_mutation() {
        for foreign in [false, true] {
            let (mut documents, manifest, principal, alias) = fixture();
            if foreign {
                let mut private = documents[&alias].get_deep_value().to_json_value();
                private["messages"] =
                    json!([{"id":"foreign","role":"assistant","deviceId":"other","parts":[]}]);
                documents.insert(alias.clone(), materialize(&private).unwrap());
            } else {
                let doc = documents.remove(&alias).unwrap();
                documents.insert(
                    format!("{CHAT}::session::00000000-0000-4000-8000-000000000214"),
                    doc,
                );
            }
            let before: BTreeMap<_, _> = documents
                .iter()
                .map(|(id, doc)| (id.clone(), doc.get_deep_value().to_json_value()))
                .collect();
            assert!(reconcile_execution_snapshots(&mut documents, &manifest, &principal).is_err());
            assert_eq!(
                before,
                documents
                    .iter()
                    .map(|(id, doc)| (id.clone(), doc.get_deep_value().to_json_value()))
                    .collect()
            );
        }
    }

    #[test]
    fn reconciles_stream_prefixes_but_rejects_terminal_or_command_conflicts() {
        let mut old = json!({"id":"m","role":"assistant","deviceId":"device","status":"streaming","parts":[{"id":"p","kind":"text","text":"a"}]});
        let new = json!({"id":"m","role":"assistant","deviceId":"device","status":"complete","parts":[{"id":"p","kind":"text","text":"abc"}]});
        merge_message(&mut old, &new, false).unwrap();
        assert_eq!(old, new);
        let mut conflicting = new.clone();
        conflicting["parts"][0]["text"] = "different".into();
        assert!(merge_message(&mut old, &conflicting, false).is_err());
        let mut pending = json!({"id":"command","status":"pending","payload":{"kind":"interrupt"}});
        let applied = json!({"id":"command","status":"applied","payload":{"kind":"interrupt"}});
        merge_command(&mut pending, &applied).unwrap();
        assert_eq!(pending, applied);
        let mut rejected = applied;
        rejected["status"] = "rejected".into();
        assert!(merge_command(&mut pending, &rejected).is_err());
    }

    #[test]
    fn validates_provenance_before_projecting_and_preserves_publications() {
        for foreign in [false, true] {
            let (mut documents, manifest, principal, alias) = fixture();
            let record = json!({"id":format!("provenance/{alias}/message"),"schemaVersion":2,"publishedAt":1,
                "publishedBy":"owner","kind":"messageProvenance","value":{"messageId":"message","sessionId":alias,
                "authorSubject":if foreign {"foreign"} else {"owner"},"ownerDeviceId":"device","source":"local"}});
            let mut private = documents[&alias].get_deep_value().to_json_value();
            private["publications"] = json!([{"id":record["id"],"record":record}]);
            documents.insert(alias.clone(), materialize(&private).unwrap());
            let result = reconcile_execution_snapshots(&mut documents, &manifest, &principal);
            if foreign {
                assert!(result.is_err());
                assert_eq!(
                    documents[&alias].get_deep_value().to_json_value()["meta"]["chatId"],
                    alias
                );
            } else {
                result.unwrap();
                let normalized = normalized_execution_record(
                    "publications",
                    &private["publications"][0],
                    &alias,
                    CHAT,
                )
                .unwrap();
                assert_eq!(
                    documents[CHAT].get_deep_value().to_json_value()["publications"][0],
                    normalized
                );
                assert_eq!(normalized["record"]["value"]["sessionId"], CHAT);
            }
        }
    }

    #[test]
    fn workspace_aliases_preserve_native_binding_and_reject_conflicts() {
        for conflict in [false, true] {
            let (mut documents, manifest, principal, alias) = fixture();
            let mut root = documents["workspace3"].get_deep_value().to_json_value();
            root["chats"][&alias] =
                json!({"id":alias,"deviceId":"device","harnessSessionId":"native"});
            root["sessionRefs"] =
                json!({(format!("5:owner:{alias}")):{"chatId":alias,"userId":"owner"}});
            if conflict {
                root["chats"][CHAT]["harnessSessionId"] = "other".into();
            }
            documents.insert("workspace3".into(), materialize_workspace(&root).unwrap());
            let result = reconcile_execution_snapshots(&mut documents, &manifest, &principal);
            if conflict {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap().get(&alias).map(String::as_str), Some(CHAT));
                let root = documents["workspace3"].get_deep_value().to_json_value();
                assert_eq!(root["chats"][CHAT]["harnessSessionId"], "native");
                assert!(root["chats"].get(&alias).is_none());
                assert_eq!(
                    root["sessionRefs"][format!("5:owner:{CHAT}")]["chatId"],
                    CHAT
                );
            }
        }
    }

    #[test]
    fn journal_projection_is_in_place_and_ignores_other_chats() {
        let alias = format!("{CHAT}::session::{CHAT}");
        let other = "00000000-0000-4000-8000-000000000214";
        for (container, mut row) in [
            ("chats", json!({"id":other,"title":alias})),
            ("sessionRefs", json!({"chatId":other,"userId":"owner"})),
            (
                "publications",
                json!({"id":"provenance","kind":"messageProvenance","value":{"sessionId":other}}),
            ),
        ] {
            let before = row.clone();
            normalize_execution_record(container, &mut row, &alias, CHAT).unwrap();
            assert_eq!(row, before);
        }
        let mut reference = json!({"chatId":alias,"userId":"owner"});
        normalize_execution_record("sessionRefs", &mut reference, &alias, CHAT).unwrap();
        assert_eq!(reference["chatId"], CHAT);
        let mut publication = json!({"id":format!("provenance/{alias}/message"),"kind":"messageProvenance",
            "value":{"sessionId":alias,"authorSubject":"owner"}});
        normalize_execution_record("publications", &mut publication, &alias, CHAT).unwrap();
        assert_eq!(publication["id"], format!("provenance/{CHAT}/message"));
        assert_eq!(publication["value"]["authorSubject"], "owner");
        let projected = publication.clone();
        normalize_execution_record("publications", &mut publication, &alias, CHAT).unwrap();
        assert_eq!(publication, projected);
    }
}

/// Project understood journal identity fields, never owner fields. The caller
/// compares this row to an independently owner-checked snapshot and keeps raw bytes.
pub(crate) fn normalize_execution_record(
    container: &str,
    row: &mut Value,
    alias: &str,
    canonical: &str,
) -> Result<()> {
    if canonical_execution_id(alias) != Some(canonical) {
        return Err(unsafe_data("invalid journal execution alias"));
    }
    let project = |value: &mut Value| {
        if value.as_str().is_some_and(|id| {
            id.starts_with(canonical) && canonical_execution_id(id) == Some(canonical)
        }) {
            *value = canonical.into();
        }
    };
    match container {
        "chats" => {
            if let Some(id) = row.get_mut("id") {
                project(id);
            }
        }
        "sessions" | "sessionRefs" | "worktreeDeletions" => {
            if let Some(id) = row.get_mut("chatId") {
                project(id);
            }
        }
        "messages" => {
            if let Some(peer) = row
                .get_mut("peerMessage")
                .and_then(|peer| peer.get_mut("sourceChatId"))
            {
                project(peer);
            }
        }
        "publications" | "agentSessions" => {
            let wrapped = row.get("record").is_some();
            let record = if wrapped {
                &mut row["record"]
            } else {
                &mut *row
            };
            match record.get("kind").and_then(Value::as_str) {
                Some("messageProvenance") => {
                    let Some(old) = record
                        .get("value")
                        .and_then(|value| value.get("sessionId"))
                        .and_then(Value::as_str)
                    else {
                        return Ok(());
                    };
                    if old != canonical
                        && (!old.starts_with(canonical)
                            || canonical_execution_id(old) != Some(canonical))
                    {
                        return Ok(());
                    }
                    let id = record
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| unsafe_data("journal publication has no ID"))?;
                    if old == canonical && !id.contains(alias) {
                        return Ok(());
                    }
                    let id = if old != canonical {
                        id.replace(old, canonical)
                    } else {
                        id.to_owned()
                    };
                    let id = id.replace(alias, canonical);
                    record["value"]["sessionId"] = canonical.into();
                    record["id"] = id.into();
                }
                Some("agentSession") => {
                    if let Some(value) = record.get_mut("value") {
                        for field in ["sessionId", "chatId"] {
                            if let Some(id) = value.get_mut(field) {
                                project(id);
                            }
                        }
                    }
                    return Ok(()); // Agent membership projection does not rename publication IDs.
                }
                _ => return Ok(()),
            }
            if wrapped {
                row["id"] = row["record"]["id"].clone();
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn normalized_execution_record(
    container: &str,
    value: &Value,
    alias: &str,
    canonical: &str,
) -> Result<Value> {
    let mut row = value.clone();
    normalize_execution_record(container, &mut row, alias, canonical)?;
    Ok(row)
}

fn normalize_workspace(
    original: &LoroDoc,
    workspace: &mut Value,
    aliases: &mut BTreeMap<String, String>,
    manifest: &CrewBackupManifest,
    principal: &CollaborationPrincipal,
) -> Result<()> {
    let owner_devices: BTreeMap<String, String> = workspace
        .get("chats")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|rows| rows.iter())
        .filter_map(|(id, row)| {
            row.get("deviceId")
                .and_then(Value::as_str)
                .map(|device| (id.clone(), device.to_owned()))
        })
        .collect();
    for container in ["chats", "sessions", "sessionRefs", "worktreeDeletions"] {
        let Some(value) = workspace.get_mut(container) else {
            continue;
        };
        let rows = value
            .as_object_mut()
            .ok_or_else(|| unsafe_data("invalid workspace alias container"))?;
        let keys: Vec<_> = rows.keys().cloned().collect();
        for key in keys {
            let row = &rows[&key];
            let identity = if container == "chats" { "id" } else { "chatId" };
            let id = row
                .get(identity)
                .and_then(Value::as_str)
                .ok_or_else(|| unsafe_data("workspace row has no identity"))?;
            let prefix = if container == "sessionRefs" {
                let user = row
                    .get("userId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| unsafe_data("session ref has no user"))?;
                format!("{}:{user}:", user.len())
            } else {
                String::new()
            };
            let key_id = key
                .strip_prefix(&prefix)
                .ok_or_else(|| unsafe_data("invalid session reference key"))?;
            if !key_id.contains("::session::") && !id.contains("::session::") {
                continue;
            }
            let canonical = canonical_execution_id(key_id)
                .or_else(|| canonical_execution_id(id))
                .ok_or_else(|| {
                    unsafe_data("distinct child workspace identity requires reattachment")
                })?
                .to_owned();
            for retained in [key_id, id] {
                if retained != canonical
                    && canonical_execution_id(retained) != Some(canonical.as_str())
                {
                    return Err(unsafe_data("workspace alias identity conflicts"));
                }
                if retained != canonical {
                    aliases.insert(retained.into(), canonical.clone());
                }
            }
            if !manifest.chats.contains_key(&canonical) {
                return Err(unsafe_data(
                    "workspace alias has no canonical manifest binding",
                ));
            }
            for field in ["userId", "ownerSubject", "ownerPrincipal"] {
                if row
                    .get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|owner| owner != principal.subject)
                {
                    return Err(unsafe_data("foreign workspace alias owner"));
                }
            }
            if let Some(environment) = row.get("environment").filter(|value| !value.is_null()) {
                let environment: zeron_proto::SessionEnvironment =
                    serde_json::from_value(environment.clone())?;
                if environment.owner_principal != principal.subject
                    || environment.scope.project_id != principal.project_id
                    || environment.scope.deployment_id != principal.deployment_id
                    || environment.scope.session_id.as_deref() != Some(canonical.as_str())
                {
                    return Err(unsafe_data(
                        "workspace alias environment requires authenticated reattachment",
                    ));
                }
            }
            let owner_field = match container {
                "sessionRefs" => "userId",
                "worktreeDeletions" => "ownerDeviceId",
                _ => "deviceId",
            };
            if row
                .get(owner_field)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(unsafe_data("workspace alias lacks owner identity"));
            }
            if container != "sessionRefs"
                && row.get(owner_field).and_then(Value::as_str)
                    != owner_devices.get(&canonical).map(String::as_str)
            {
                return Err(unsafe_data(
                    "workspace alias device differs from canonical owner",
                ));
            }
            let alias = if canonical_execution_id(key_id).is_some() {
                key_id
            } else {
                id
            };
            let normalized = normalized_execution_record(container, row, alias, &canonical)?;
            let new_key = format!("{prefix}{canonical}");
            if key == new_key {
                rows.insert(new_key, normalized);
                continue;
            }
            if let Some(current) = rows.get_mut(&new_key) {
                let current = current
                    .as_object_mut()
                    .ok_or_else(|| unsafe_data("invalid canonical workspace row"))?;
                let incoming = normalized.as_object().expect("normalized workspace row");
                for field in [
                    "id",
                    "chatId",
                    "userId",
                    "deviceId",
                    "ownerSubject",
                    "ownerDeviceId",
                    "ownerPrincipal",
                    "projectId",
                    "deploymentId",
                ] {
                    if current.get(field) != incoming.get(field) {
                        return Err(unsafe_data("workspace alias owner/scope conflicts"));
                    }
                }
                let newer_status = container == "sessions"
                    && incoming.get("updatedAt").and_then(Value::as_i64)
                        > current.get("updatedAt").and_then(Value::as_i64);
                for (field, value) in incoming {
                    if let Some(existing) = current.get(field) {
                        // Native associations and grants never use display-field precedence.
                        if matches!(
                            field.as_str(),
                            "harnessSessionId"
                                | "harnessSessionCwd"
                                | "forkFrom"
                                | "environment"
                                | "startup"
                                | "crew"
                                | "config"
                        ) && existing != value
                        {
                            return Err(unsafe_data(format!(
                                "workspace alias native/authority conflict: {field}"
                            )));
                        }
                        if newer_status
                            && matches!(field.as_str(), "status" | "startedAt" | "updatedAt")
                        {
                            current.insert(field.clone(), value.clone());
                        }
                    } else {
                        if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                            original.get_map(container).get(&new_key)
                            && map.get_last_editor(field).is_some()
                        {
                            return Err(unsafe_data(
                                "workspace alias conflicts with canonical field tombstone",
                            ));
                        }
                        current.insert(field.clone(), value.clone());
                    }
                }
            } else {
                if container == "chats"
                    || original
                        .get_map(container)
                        .get_last_editor(&new_key)
                        .is_some()
                {
                    return Err(unsafe_data(
                        "workspace alias has no live canonical owner row",
                    ));
                }
                rows.insert(new_key, normalized);
            }
            rows.remove(&key);
        }
    }
    Ok(())
}

fn materialize_workspace(root: &Value) -> Result<LoroDoc> {
    let doc = LoroDoc::new();
    for (container, value) in root
        .as_object()
        .ok_or_else(|| unsafe_data("invalid workspace root"))?
    {
        let fields = value
            .as_object()
            .ok_or_else(|| unsafe_data("invalid workspace map"))?;
        let parent = doc.get_map(container.as_str());
        if container == "meta" {
            write_map(&parent, fields)?;
            continue;
        }
        for (key, row) in fields {
            if let Some(fields) = row.as_object() {
                write_map(&parent.insert_container(key, LoroMap::new())?, fields)?;
            } else {
                parent.insert(key, LoroValue::from(row.clone()))?;
            }
        }
    }
    doc.commit();
    Ok(doc)
}
