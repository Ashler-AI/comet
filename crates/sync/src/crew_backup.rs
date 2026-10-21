//! Offline export of a stopped-writer COPY, never of the live Crew profile.
//! The caller must obtain an explicit stopped-writer confirmation before calling.
//! Native roots are copied OMP homes below that copy (with `sessions/` and `blobs/`).
//! The original profile path is only an attachment alias; no files are read there.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

use loro::{LoroDoc, ToJson};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeron_proto::{
    AgentSessionSource, CollaborationPrincipal, CollaborationScope, CrewChatMetadata, HarnessId,
    PublicationRecord, PublicationValue,
};

use crate::crew_import::{CrewBackupManifest, CrewImportError};

fn unsafe_data(message: impl Into<String>) -> CrewImportError {
    CrewImportError::Unsafe(message.into())
}

fn relative(path: &Path) -> Result<&str, CrewImportError> {
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(unsafe_data(format!(
            "unsafe relative path: {}",
            path.display()
        )));
    }
    path.to_str()
        .ok_or_else(|| unsafe_data("non-UTF8 backup path"))
}

// Check the supplied spelling as well as the resolved path: canonicalization
// alone would silently authorize a symlink into the original profile.
fn no_links(path: &Path) -> Result<(), CrewImportError> {
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        if fs::symlink_metadata(ancestor)?.file_type().is_symlink() {
            return Err(unsafe_data(format!(
                "symlink is not a copied file: {}",
                ancestor.display()
            )));
        }
    }
    Ok(())
}

pub(crate) fn file(path: &Path) -> Result<File, CrewImportError> {
    no_links(path)?;
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() {
        return Err(unsafe_data("expected a regular copied file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.nlink() != 1 {
            return Err(unsafe_data("hard-linked files are not independent copies"));
        }
    }
    let file = File::open(path)?;
    let after = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(unsafe_data("copied file changed during open"));
        }
    }
    if !after.is_file() || before.len() != after.len() || before.modified()? != after.modified()? {
        return Err(unsafe_data("copied file changed during open"));
    }
    Ok(file)
}

pub(crate) fn hash(path: &Path) -> Result<String, CrewImportError> {
    let mut source = file(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn inventory(
    root: &Path,
    directory: &Path,
    result: &mut BTreeSet<PathBuf>,
) -> Result<(), CrewImportError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() || (!kind.is_dir() && !kind.is_file()) {
            return Err(unsafe_data(format!(
                "nonregular copied entry: {}",
                entry.path().display()
            )));
        }
        if kind.is_dir() {
            inventory(root, &entry.path(), result)?;
        } else {
            let path = entry.path();
            let path = path
                .strip_prefix(root)
                .map_err(|_| unsafe_data("copy escaped its root"))?;
            relative(path)?;
            result.insert(path.to_path_buf());
        }
    }
    Ok(())
}

fn forbidden(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("credential")
        || lower.contains("token")
        || lower.contains("auth.json")
        || lower.contains("keychain")
        || lower.contains("device-bootstrap")
        || lower.ends_with("-wal")
        || lower.ends_with("-shm")
        || lower.ends_with(".uploading")
        || lower.split('/').any(|part| {
            matches!(
                part,
                "agent-accounts"
                    | "session.json"
                    | "scaffold-session.json"
                    | "scaffold-device-session.json"
            )
        })
        || lower.starts_with("uploads/tmp/")
        || lower.contains("/uploads/tmp/")
}

fn nonsecret_settings(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.iter().all(|(key, value)| {
            let key = key.to_ascii_lowercase().replace(['_', '-'], "");
            ![
                "credential",
                "token",
                "password",
                "secret",
                "privatekey",
                "authorization",
                "cookie",
                "apikey",
            ]
            .iter()
            .any(|secret| key.contains(secret))
                && nonsecret_settings(value)
        }),
        Value::Array(values) => values.iter().all(nonsecret_settings),
        _ => true,
    }
}

fn checked_scope(
    value: &Value,
    id: &str,
    principal: &CollaborationPrincipal,
) -> Result<(), CrewImportError> {
    let session = value.get("session").and_then(Value::as_str);
    let canonical = crate::crew_import_aliases::canonical_execution_id(id);
    if value.get("project").and_then(Value::as_str) != Some(principal.project_id.as_str())
        || value.get("principal").and_then(Value::as_str) != Some(principal.subject.as_str())
        || (id != "workspace3"
            && ((session != Some(id) && (canonical.is_none() || session != canonical))
                || value.get("deployment").and_then(Value::as_str)
                    != principal.deployment_id.as_deref()))
    {
        return Err(unsafe_data(format!(
            "unproven or conflicting legacy scope: {id}"
        )));
    }
    Ok(())
}

fn scoped_metadata(id: &str, scope: &Value) -> CrewChatMetadata {
    CrewChatMetadata {
        owner_subject: scope["principal"].as_str().unwrap().into(),
        scope: CollaborationScope {
            project_id: scope["project"].as_str().unwrap().into(),
            deployment_id: scope
                .get("deployment")
                .and_then(Value::as_str)
                .map(str::to_owned),
            session_id: Some(id.into()),
            unknown: BTreeMap::new(),
        },
        session_id: id.into(),
        environment: None,
        agent_route: None,
        original_epoch_history: None,
    }
}

fn owned_publication(
    value: &Value,
    id: &str,
    device: &str,
    principal: &CollaborationPrincipal,
) -> Result<bool, CrewImportError> {
    let record: PublicationRecord = serde_json::from_value(value.clone())?;
    zeron_doc::validate_publication(&record)?;
    let PublicationValue::AgentSession(session) = record.value else {
        return Ok(false);
    };
    if session.session_id != id
        || session.chat_id != id
        || session.owner_subject != principal.subject
        || session.owner_device_id != device
        || session.environment.is_some()
        || session.source != AgentSessionSource::Local
        || session.harness != Some(HarnessId::Omp)
    {
        return Err(unsafe_data(format!(
            "shared, remote, foreign or non-OMP agent session: {id}"
        )));
    }
    Ok(true)
}

fn source_documents(db: &Connection) -> Result<BTreeMap<String, LoroDoc>, CrewImportError> {
    let mut documents = BTreeMap::new();
    for row in db
        .prepare("SELECT doc_id,bytes FROM snapshots ORDER BY doc_id")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
    {
        let (id, bytes) = row?;
        if id.starts_with("recovery-original/") {
            continue;
        } // Opaque evidence remains in the original SQLite file.
        let doc = LoroDoc::new();
        let status = doc
            .import(&bytes)
            .map_err(|error| unsafe_data(format!("snapshot {id}: {error}")))?;
        if status.pending.is_some() || doc.is_detached() || doc.state_vv() != doc.oplog_vv() {
            return Err(unsafe_data(format!(
                "snapshot has unresolved causal operations: {id}"
            )));
        }
        documents.insert(id, doc);
    }
    Ok(documents)
}

fn metadata(
    db: &Connection,
    principal: &CollaborationPrincipal,
    remote: Option<&crate::crew_remote_backup::RemoteBackupContext>,
    desktop: Option<&crate::crew_desktop_backup::DesktopSourceProof>,
    assets: &mut crate::crew_backup_assets::RequiredAssets,
    terminals: &mut crate::crew_backup_terminal::TerminalBackups,
    temporary_parent: &Path,
    native_archive_ids: &BTreeSet<String>,
) -> Result<
    (
        BTreeMap<String, CrewChatMetadata>,
        BTreeMap<String, String>,
        BTreeSet<String>,
    ),
    CrewImportError,
> {
    let mut scopes = BTreeMap::new();
    for row in db
        .prepare("SELECT doc_id,scope FROM document_scopes")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
    {
        let (id, scope) = row?;
        let value: Value = serde_json::from_str(&scope)?;
        if let Some(proof) = desktop {
            proof.validate_source_scope(&id, &value, principal)?;
        } else {
            checked_scope(&value, &id, principal)?;
        }
        scopes.insert(id, value);
    }
    if !scopes.contains_key("workspace3") {
        return Err(unsafe_data("workspace has no persisted scope"));
    }
    let projected_metadata = |id: &str, scope: &Value| {
        let mut metadata = scoped_metadata(id, scope);
        if desktop.is_some() && zeron_proto::is_cloud_session_id(id) {
            metadata.scope.deployment_id = principal.deployment_id.clone();
        }
        metadata
    };
    let mut documents = source_documents(db)?;
    let mut associated_ids = native_archive_ids.clone();
    if let Some(proof) = desktop {
        associated_ids.extend(proof.source_witnesses.keys().cloned());
    }
    let inert =
        crate::crew_import_aliases::inert_bootstrap_snapshots(db, &documents, &associated_ids)?;
    if desktop.is_some_and(|proof| proof.inert_bootstrap_snapshots != inert) {
        return Err(unsafe_data(
            "desktop inert-bootstrap disposition differs from the complete reconciled source",
        ));
    }
    for id in inert.keys() {
        documents.remove(id);
    } // Exact snapshots remain in the inventoried SQLite source.
    // This binding is only input to the shared alias validator, not ownership
    // evidence. Explicit owner publications are required below before export.
    let provisional = CrewBackupManifest {
        version: 1,
        writer_stopped: true,
        source_profile_root: String::new(),
        owner_subject: principal.subject.clone(),
        project_id: principal.project_id.clone(),
        deployment_id: principal.deployment_id.clone(),
        files: BTreeMap::new(),
        chats: scopes
            .iter()
            .filter(|(id, _)| {
                zeron_proto::is_cloud_session_id(id) || zeron_proto::is_local_history_chat(id)
            })
            .map(|(id, scope)| (id.clone(), projected_metadata(id, scope)))
            .collect(),
        native_histories: BTreeMap::new(),
        attachment_aliases: BTreeMap::new(),
        desktop_source: None,
        original_epoch_histories: BTreeMap::new(),
    };
    crate::crew_import_aliases::reconcile_execution_snapshots(
        &mut documents,
        &provisional,
        principal,
    )?;
    let mut documents: BTreeMap<_, _> = documents
        .into_iter()
        .map(|(id, doc)| (id, doc.get_deep_value().to_json_value()))
        .collect();
    let workspace = documents
        .remove("workspace3")
        .ok_or_else(|| unsafe_data("missing workspace3"))?;
    if remote.is_some() || desktop.is_some() {
        assets.collect(&workspace)?;
    }
    if workspace
        .pointer("/meta/schemaVersion")
        .and_then(Value::as_u64)
        != Some(3)
    {
        return Err(unsafe_data("unsupported legacy workspace schema"));
    }
    let rows = workspace
        .get("chats")
        .and_then(Value::as_object)
        .ok_or_else(|| unsafe_data("workspace has no chat map"))?;
    terminals.prepare(rows, &documents, principal, temporary_parent)?;
    if let Some(refs) = workspace.get("sessionRefs") {
        for row in refs
            .as_object()
            .ok_or_else(|| unsafe_data("invalid sessionRefs"))?
            .values()
        {
            let terminal_id = row
                .get("chatId")
                .and_then(Value::as_str)
                .filter(|id| terminals.metadata.contains_key(*id));
            if row.get("userId").and_then(Value::as_str) != Some(principal.subject.as_str())
                || (remote.is_none()
                    && terminal_id.is_none()
                    && row.get("environment").is_some_and(|value| !value.is_null()))
                || row.get("startup").is_some_and(|value| !value.is_null())
                || !row
                    .get("chatId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| documents.contains_key(id))
            {
                return Err(unsafe_data(
                    "shared, remote or unresolved session membership",
                ));
            }
            if let Some(id) = terminal_id {
                if let Some(environment) = row.get("environment").filter(|value| !value.is_null()) {
                    terminals.validate_ref_environment(id, environment)?;
                }
            }
            if let Some(remote) = remote {
                if row.get("chatId").and_then(Value::as_str)
                    != Some(remote.logical_session_id.as_str())
                {
                    return Err(unsafe_data("remote membership differs from fenced session"));
                }
                if let Some(environment) = row.get("environment").filter(|value| !value.is_null()) {
                    remote.environment(&serde_json::from_value(environment.clone())?)?;
                }
            }
        }
    }
    let mut chats = BTreeMap::new();
    let mut native = BTreeMap::new();
    let mut required_native = BTreeSet::new();
    for (id, doc) in &documents {
        if remote.is_some() || desktop.is_some() {
            assets.collect(doc)?;
        }
        let local = zeron_proto::is_local_history_chat(id);
        if (!zeron_proto::is_cloud_session_id(id) && !local)
            || !scopes.contains_key(id)
            || doc.pointer("/meta/chatId").and_then(Value::as_str) != Some(id.as_str())
            || !matches!(
                doc.pointer("/meta/schemaVersion").and_then(Value::as_u64),
                Some(1 | 2)
            )
        {
            return Err(unsafe_data(format!(
                "unknown, unscoped or mismatched legacy snapshot: {id}"
            )));
        }
        let row = rows
            .get(id)
            .ok_or_else(|| unsafe_data(format!("session has no host row: {id}")))?;
        let original_epoch = terminals.metadata.get(id);
        if row.get("id").and_then(Value::as_str) != Some(id.as_str())
            || (original_epoch.is_none()
                && (row.pointer("/config/harness").and_then(Value::as_str) != Some("omp")
                    || row.get("forkFrom").is_some_and(|value| !value.is_null())))
        {
            return Err(unsafe_data(format!("non-OMP or unresolved host row: {id}")));
        }
        let device = row
            .get("deviceId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| unsafe_data(format!("host identity missing: {id}")))?;
        let metadata = if let Some(metadata) = original_epoch {
            metadata.clone()
        } else if let Some(remote) = remote {
            remote.document(id, device, row, doc)?
        } else {
            projected_metadata(id, &scopes[id])
        };
        let mut owned = remote.is_some() || original_epoch.is_some();
        if remote.is_none() && original_epoch.is_none() {
            if let Some(crew) = row.get("crew").filter(|value| !value.is_null()) {
                if local {
                    return Err(unsafe_data(
                        "local native history cannot carry cloud Crew authority",
                    ));
                }
                let mut existing: CrewChatMetadata = serde_json::from_value(crew.clone())?;
                existing.validate().map_err(unsafe_data)?;
                if desktop.is_some() {
                    let source = &scopes[id]; // Already checked against the exact original witness above.
                    if Some(existing.owner_subject.as_str())
                        != source.get("principal").and_then(Value::as_str)
                        || Some(existing.scope.project_id.as_str())
                            != source.get("project").and_then(Value::as_str)
                        || existing.scope.deployment_id.as_deref()
                            != source.get("deployment").and_then(Value::as_str)
                        || existing.scope.session_id.as_deref() != Some(id.as_str())
                    {
                        return Err(unsafe_data(
                            "existing Crew metadata differs from original desktop source scope",
                        ));
                    }
                    existing.scope.deployment_id = metadata.scope.deployment_id.clone();
                }
                if existing != metadata {
                    return Err(unsafe_data(format!(
                        "conflicting or remote Crew metadata: {id}"
                    )));
                }
                owned = true;
            }
            if let Some(sessions) = doc.get("agentSessions") {
                for (key, value) in sessions
                    .as_object()
                    .ok_or_else(|| unsafe_data("invalid agentSessions"))?
                {
                    if key != id || !owned_publication(value, id, device, principal)? {
                        return Err(unsafe_data(format!(
                            "unproven shared agent membership: {id}"
                        )));
                    }
                    owned = true;
                }
            }
            if let Some(publications) = doc.get("publications") {
                for row in publications
                    .as_array()
                    .ok_or_else(|| unsafe_data("invalid publications"))?
                {
                    owned |= owned_publication(
                        row.get("record")
                            .ok_or_else(|| unsafe_data("publication has no record"))?,
                        id,
                        device,
                        principal,
                    )?;
                }
            }
        }
        if !owned {
            return Err(unsafe_data(format!(
                "no explicit owner evidence for {id}; cache scope is not ownership"
            )));
        }
        metadata.validate().map_err(unsafe_data)?;
        if local
            && !row
                .get("harnessSessionId")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
        {
            return Err(unsafe_data(
                "local native history has no native resume identity",
            ));
        }
        if let Some(native_id) = row
            .get("harnessSessionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let cwd = row
                .get("harnessSessionCwd")
                .or_else(|| original_epoch.and_then(|_| row.get("cwd")))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| unsafe_data(format!("native session has no cwd: {id}")))?;
            if let Some(previous) = native.insert(native_id.into(), cwd.into()) {
                if previous != cwd {
                    return Err(unsafe_data("native ID has conflicting cwd bindings"));
                }
            }
            if original_epoch.is_none() {
                required_native.insert(native_id.to_owned());
            }
        }
        chats.insert(id.clone(), metadata);
    }
    if rows.len() != chats.len() {
        return Err(unsafe_data("host rows without scoped session snapshots"));
    }
    if remote.is_some() && chats.len() != 1 {
        return Err(unsafe_data(
            "remote backup must contain exactly its fenced session",
        ));
    }
    Ok((chats, native, required_native))
}

fn header(path: &Path) -> Result<Option<(String, String)>, CrewImportError> {
    for line in BufReader::new(file(path)?.take(64 * 1024)).lines() {
        let line = line?;
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            if value.get("type").and_then(Value::as_str) == Some("session") {
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                let cwd = value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                return id
                    .zip(cwd)
                    .map(|(id, cwd)| Some((id.into(), cwd.into())))
                    .ok_or_else(|| unsafe_data("native journal has an incomplete header"));
            }
        }
    }
    Ok(None)
}

struct CopiedNativeRoot {
    path: PathBuf,
    // Exact copied relative path and session header identity/cwd, never filename guesses.
    journals: Vec<(PathBuf, String, String)>,
}

fn native_sources(
    source: &Path,
    supplied: &[PathBuf],
) -> Result<(Vec<CopiedNativeRoot>, BTreeSet<String>), CrewImportError> {
    let mut roots: Vec<CopiedNativeRoot> = Vec::with_capacity(supplied.len());
    let mut identities = BTreeSet::new();
    for root in supplied {
        no_links(root)?;
        let root = root.canonicalize()?;
        if root == source
            || !root.starts_with(source)
            || roots.iter().any(|previous| {
                root.starts_with(&previous.path) || previous.path.starts_with(&root)
            })
        {
            return Err(unsafe_data(
                "native roots must be disjoint copied OMP homes below the copied profile",
            ));
        }
        let mut files = BTreeSet::new();
        inventory(&root, &root.join("sessions"), &mut files)?;
        let mut journals = Vec::new();
        for relative in files {
            if relative
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("jsonl")
            {
                continue;
            }
            if let Some((id, cwd)) = header(&root.join(&relative))? {
                identities.insert(id.clone());
                journals.push((relative, id, cwd));
            }
        }
        roots.push(CopiedNativeRoot {
            path: root,
            journals,
        });
    }
    Ok((roots, identities))
}

/// Inspect a stopped-writer independent copy without publishing or modifying it.
/// Results describe inert evidence only; they never grant session authority.
pub fn inspect_inert_bootstraps(
    source_backup_root: &Path,
    database_relative: &Path,
    native_history_roots: &[PathBuf],
) -> Result<Value, CrewImportError> {
    if !source_backup_root.is_absolute() {
        return Err(unsafe_data(
            "inspection requires an absolute copied source root",
        ));
    }
    relative(database_relative)?;
    no_links(source_backup_root)?;
    let source = source_backup_root.canonicalize()?;
    let database = source.join(database_relative);
    for suffix in ["-wal", "-shm"] {
        let sidecar = database.with_file_name(format!(
            "{}{suffix}",
            database.file_name().unwrap().to_string_lossy()
        ));
        match fs::symlink_metadata(sidecar) {
            Ok(_) => {
                return Err(unsafe_data(
                    "inspection requires a checkpointed copy without WAL/SHM sidecars",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let before = hash(&database)?;
    let (_, native_ids) = native_sources(&source, native_history_roots)?;
    let uri = format!(
        "file:{}?immutable=1",
        percent_encoding::utf8_percent_encode(
            database
                .to_str()
                .ok_or_else(|| unsafe_data("non-UTF8 source database path"))?,
            percent_encoding::NON_ALPHANUMERIC
        )
    );
    let db = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(unsafe_data("copied SQLite integrity check failed"));
    }
    let documents = source_documents(&db)?;
    let inert =
        crate::crew_import_aliases::inert_bootstrap_snapshots(&db, &documents, &native_ids)?;
    drop(db);
    if hash(&database)? != before {
        return Err(unsafe_data(
            "source database changed during read-only inspection",
        ));
    }
    let dispositions: BTreeMap<_, _> = inert
        .into_iter()
        .map(|(id, digest)| {
            (
                id,
                serde_json::json!({"snapshotSha256":digest,"disposition":"inert-bootstrap"}),
            )
        })
        .collect();
    Ok(
        serde_json::json!({"version":1,"sourceDatabaseSha256":before,"inertBootstrapSnapshots":dispositions}),
    )
}

fn blob_refs(value: &Value, hashes: &mut BTreeSet<String>) -> Result<(), CrewImportError> {
    match value {
        Value::String(value) => {
            let reference = value.strip_prefix("blob:sha256:").or_else(|| {
                value
                    .strip_prefix("data:")?
                    .split_once(";base64,blob:sha256:")
                    .map(|(_, hash)| hash)
            });
            if let Some(hash) = reference {
                if hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(unsafe_data("malformed native blob reference"));
                }
                hashes.insert(hash.into());
            }
        }
        Value::Array(values) => {
            for value in values {
                blob_refs(value, hashes)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                blob_refs(value, hashes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn plan_file(
    plan: &mut BTreeMap<String, PathBuf>,
    target: &Path,
    source: PathBuf,
) -> Result<(), CrewImportError> {
    let target = relative(target)?;
    if forbidden(target) {
        return Err(unsafe_data(format!(
            "credential or live sidecar in export: {target}"
        )));
    }
    if let Some(previous) = plan.insert(target.into(), source.clone()) {
        if previous != source {
            return Err(unsafe_data(format!("ambiguous export path: {target}")));
        }
    }
    Ok(())
}

/// Export an independently copied, checkpointed profile after all its writers
/// have stopped. `principal` must come from authenticated RPC, not caller JSON.
/// `source_profile_root` is an absolute original attachment alias only. All
/// native roots must be explicitly copied OMP homes inside `source_backup_root`.
/// Failure never deletes source evidence; an interrupted destination has no
/// manifest and must not be imported. Existing destinations are never reused.
pub fn export_legacy_crew_backup(
    source_profile_root: &Path,
    source_backup_root: &Path,
    database_relative: &Path,
    destination: &Path,
    native_history_roots: &[PathBuf],
    principal: &CollaborationPrincipal,
) -> Result<CrewBackupManifest, CrewImportError> {
    match export_crew_backup(
        source_profile_root,
        source_backup_root,
        database_relative,
        destination,
        native_history_roots,
        principal,
        None,
        None,
        None,
        false,
        None,
    )? {
        BackupOutput::Backup(manifest) => Ok(manifest),
        BackupOutput::Inventory(_) => Err(unsafe_data("unexpected local inventory result")),
    }
}

pub fn export_desktop_crew_backup(
    source_profile_root: &Path,
    source_backup_root: &Path,
    database_relative: &Path,
    destination: &Path,
    native_history_roots: &[PathBuf],
    principal: &CollaborationPrincipal,
    context: &crate::crew_desktop_backup::DesktopSourceContext,
    storage_directory: &Path,
    attachment_root: Option<&Path>,
    terminal_authority_root: Option<&Path>,
) -> Result<CrewBackupManifest, CrewImportError> {
    match export_crew_backup(
        source_profile_root,
        source_backup_root,
        database_relative,
        destination,
        native_history_roots,
        principal,
        None,
        Some((context, storage_directory)),
        attachment_root,
        false,
        terminal_authority_root,
    )? {
        BackupOutput::Backup(manifest) => Ok(manifest),
        BackupOutput::Inventory(_) => Err(unsafe_data("unexpected desktop inventory result")),
    }
}

pub fn inventory_desktop_crew_backup(
    source_profile_root: &Path,
    source_backup_root: &Path,
    database_relative: &Path,
    destination: &Path,
    native_history_roots: &[PathBuf],
    principal: &CollaborationPrincipal,
    context: &crate::crew_desktop_backup::DesktopSourceContext,
    storage_directory: &Path,
    attachment_root: Option<&Path>,
    terminal_authority_root: Option<&Path>,
) -> Result<crate::crew_backup_assets::AttachmentInventory, CrewImportError> {
    match export_crew_backup(
        source_profile_root,
        source_backup_root,
        database_relative,
        destination,
        native_history_roots,
        principal,
        None,
        Some((context, storage_directory)),
        attachment_root,
        true,
        terminal_authority_root,
    )? {
        BackupOutput::Inventory(inventory) => Ok(inventory),
        BackupOutput::Backup(_) => Err(unsafe_data(
            "desktop inventory unexpectedly published a backup",
        )),
    }
}

pub(crate) enum BackupOutput {
    Backup(CrewBackupManifest),
    Inventory(crate::crew_backup_assets::AttachmentInventory),
}

pub(crate) fn export_crew_backup(
    source_profile_root: &Path,
    source_backup_root: &Path,
    database_relative: &Path,
    destination: &Path,
    native_history_roots: &[PathBuf],
    principal: &CollaborationPrincipal,
    remote: Option<(&crate::crew_remote_backup::RemoteBackupContext, &Path)>,
    desktop: Option<(&crate::crew_desktop_backup::DesktopSourceContext, &Path)>,
    attachment_root: Option<&Path>,
    inventory_only: bool,
    terminal_authority_root: Option<&Path>,
) -> Result<BackupOutput, CrewImportError> {
    if remote.is_some() && desktop.is_some() {
        return Err(unsafe_data(
            "one explicit source authority mode is required",
        ));
    }
    let acquire_assets = remote.is_some() || desktop.is_some();
    if !source_profile_root.is_absolute()
        || source_profile_root
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        || principal.subject.trim().is_empty()
        || principal.project_id.trim().is_empty()
        || principal.session_id.is_some()
    {
        return Err(unsafe_data(
            "absolute original alias and verified project owner are required",
        ));
    }
    relative(database_relative)?;
    no_links(source_backup_root)?;
    let source = source_backup_root.canonicalize()?;
    // Resolve only path metadata on the original, never open its contents.
    let original = if source_profile_root.exists() {
        no_links(source_profile_root)?;
        source_profile_root.canonicalize()?
    } else {
        source_profile_root.to_path_buf()
    };
    if source.starts_with(&original) || original.starts_with(&source) {
        return Err(unsafe_data(
            "source must be an independent copy, not the live original",
        ));
    }
    if fs::symlink_metadata(destination).is_ok() {
        return Err(unsafe_data("export destination already exists"));
    }
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| unsafe_data("destination requires an explicit parent"))?;
    no_links(parent)?;
    let target = parent.canonicalize()?.join(
        destination
            .file_name()
            .ok_or_else(|| unsafe_data("invalid destination"))?,
    );
    if source.starts_with(&target)
        || target.starts_with(&source)
        || original.starts_with(&target)
        || target.starts_with(&original)
    {
        return Err(unsafe_data(
            "export destination overlaps source or original profile",
        ));
    }
    let mut entries = BTreeSet::new();
    inventory(&source, &source, &mut entries)?;
    let database = source.join(database_relative);
    let wal = database.with_file_name(format!(
        "{}-wal",
        database.file_name().unwrap().to_string_lossy()
    ));
    if wal.exists() && file(&wal)?.metadata()?.len() != 0 {
        return Err(unsafe_data(
            "nonempty WAL: stop writers and checkpoint before copying",
        ));
    }
    let (native_sources, native_archive_ids) = native_sources(&source, native_history_roots)?;
    let prepared = remote
        .map(|(context, snapshot)| {
            crate::crew_remote_backup::prepare_authoritative_database(
                &database,
                snapshot,
                target.parent().unwrap(),
                context,
            )
        })
        .transpose()?;
    let desktop_prepared = desktop
        .map(|(context, storage)| {
            crate::crew_desktop_backup::prepare_desktop_database(
                &database,
                storage,
                target.parent().unwrap(),
                principal,
                context,
                &native_archive_ids,
            )
        })
        .transpose()?;
    let original_database = database;
    let database = if let Some(prepared) = &prepared {
        prepared.file.path().to_owned()
    } else if let Some(prepared) = &desktop_prepared {
        prepared.file.path().to_owned()
    } else {
        original_database.clone()
    };
    let database_hash = hash(&database)?;
    let database_uri = format!(
        "file:{}?immutable=1",
        percent_encoding::utf8_percent_encode(
            database
                .to_str()
                .ok_or_else(|| unsafe_data("non-UTF8 database path"))?,
            percent_encoding::NON_ALPHANUMERIC
        )
    );
    let db = Connection::open_with_flags(
        database_uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(unsafe_data("copied SQLite integrity check failed"));
    }
    let mut assets = crate::crew_backup_assets::RequiredAssets::new(source_profile_root);
    let mut terminals = crate::crew_backup_terminal::TerminalBackups::load(
        terminal_authority_root,
        desktop_prepared.as_ref(),
    )?;
    let (chats, requested, required_native) = metadata(
        &db,
        principal,
        remote.map(|(context, _)| context),
        desktop_prepared.as_ref().map(|prepared| &prepared.proof),
        &mut assets,
        &mut terminals,
        target.parent().unwrap(),
        &native_archive_ids,
    )?;
    drop(db);
    let mut plan = BTreeMap::new();
    let mut attachment_aliases = BTreeMap::new();
    let mut attachment_inventory = crate::crew_backup_assets::AttachmentInventory::default();
    plan_file(&mut plan, Path::new("docs.sqlite3"), database.clone())?;
    if let Some((_, snapshot)) = remote {
        plan_file(
            &mut plan,
            Path::new("source-evidence/host.sqlite3"),
            original_database.clone(),
        )?;
        plan_file(
            &mut plan,
            Path::new("source-evidence/edge-storage.json"),
            snapshot.to_owned(),
        )?;
    }
    if let Some(prepared) = &desktop_prepared {
        for (relative, source) in &prepared.evidence {
            plan_file(&mut plan, Path::new(relative), source.clone())?;
        }
    }
    for (relative, source) in &terminals.evidence {
        plan_file(&mut plan, Path::new(relative), source.clone())?;
    }
    let account = database_relative.parent().unwrap();
    for path in &entries {
        let name = relative(path)?;
        if forbidden(name) {
            continue;
        }
        let local = path.strip_prefix(account).unwrap_or(path);
        if let Ok(journal) = path
            .strip_prefix(account)
            .and_then(|path| path.strip_prefix("journals"))
        {
            plan_file(
                &mut plan,
                &Path::new("legacy-journals").join(journal),
                source.join(path),
            )?;
        }
        let asset = [path.as_path(), local]
            .into_iter()
            .any(|path| path.starts_with("uploads") || path.starts_with("attachments"));
        let setting = [path.as_path(), local].into_iter().any(|path| {
            matches!(
                path.to_str(),
                Some("ui-settings.json" | "composer-defaults.json" | "repos.json")
            )
        });
        if setting
            && !nonsecret_settings(&serde_json::from_reader::<_, Value>(file(
                &source.join(path),
            )?)?)
        {
            return Err(unsafe_data(format!(
                "credential-bearing settings must not be exported: {name}"
            )));
        }
        if asset || setting {
            plan_file(&mut plan, path, source.join(path))?;
        }
    }
    let mut native_histories = BTreeMap::new();
    let mut native_hashes = BTreeMap::new();
    for (index, native) in native_sources.iter().enumerate() {
        let root = &native.path;
        for (journal, id, cwd) in &native.journals {
            let path = root.join(journal);
            let Some(expected_cwd) = requested.get(id) else {
                continue;
            };
            if cwd != expected_cwd {
                return Err(unsafe_data(format!("native journal cwd mismatch: {id}")));
            }
            let journal_hash = hash(&path)?;
            if header(&path)? != Some((id.clone(), cwd.clone())) {
                return Err(unsafe_data("native header changed during export"));
            }
            let base = PathBuf::from(format!("native-histories/{index}"));
            let output = base.join(journal);
            native_hashes.insert(relative(&output)?.to_owned(), journal_hash);
            if native_histories
                .insert(id.clone(), relative(&output)?.into())
                .is_some()
            {
                return Err(unsafe_data(format!(
                    "ambiguous native journal header: {id}"
                )));
            }
            plan_file(&mut plan, &output, path.clone())?;
            let mut blobs = BTreeSet::new();
            for line in BufReader::new(file(&path)?).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(&line)?;
                blob_refs(&value, &mut blobs)?;
                if acquire_assets {
                    assets.collect_native(&value)?;
                }
            }
            for digest in blobs {
                let copied = root.join("blobs").join(&digest);
                let output = base.join("blobs").join(&digest);
                let blob = if copied.exists() {
                    Some(copied)
                } else {
                    crate::crew_backup_assets::fetched_file(attachment_root, &digest)?
                };
                if let Some(blob) = blob {
                    if hash(&blob)? != digest {
                        return Err(unsafe_data(format!("corrupt native blob: {digest}")));
                    }
                    native_hashes.insert(relative(&output)?.to_owned(), digest);
                    plan_file(&mut plan, &output, blob)?;
                } else if acquire_assets {
                    attachment_inventory
                        .missing_attachments
                        .insert(digest.clone());
                    attachment_inventory.native_missing_blobs.push(
                        crate::crew_backup_assets::MissingAsset {
                            sha256: digest.clone(),
                            relative_path: relative(&output)?.into(),
                            reference: format!("blob:sha256:{digest}"),
                        },
                    );
                } else {
                    return Err(unsafe_data(format!("missing native blob: {digest}")));
                }
            }
        }
    }
    if required_native
        .iter()
        .any(|id| !native_histories.contains_key(id))
    {
        return Err(unsafe_data(
            "requested native journal absent from explicit copied OMP roots",
        ));
    }
    if acquire_assets {
        assets.resolve(
            &source,
            attachment_root,
            &mut plan,
            &mut attachment_aliases,
            &mut attachment_inventory,
        )?;
        if !inventory_only && !attachment_inventory.complete() {
            return Err(unsafe_data(format!(
                "unresolved source attachments: {}",
                serde_json::to_string(&attachment_inventory)?
            )));
        }
    }
    let expected = plan
        .iter()
        .map(|(target, source)| Ok((target.clone(), hash(source)?)))
        .collect::<Result<BTreeMap<_, _>, CrewImportError>>()?;
    if native_hashes
        .iter()
        .any(|(path, digest)| expected.get(path) != Some(digest))
    {
        return Err(unsafe_data("native journal or blob changed during export"));
    }
    if expected.get("docs.sqlite3") != Some(&database_hash) {
        return Err(unsafe_data("database changed during metadata extraction"));
    }
    if let Some(prepared) = &prepared {
        if expected.get("source-evidence/host.sqlite3") != Some(&prepared.host_sha256)
            || expected.get("source-evidence/edge-storage.json") != Some(&prepared.edge_sha256)
        {
            return Err(unsafe_data(
                "authoritative source evidence changed during reconciliation",
            ));
        }
    }
    if let Some(prepared) = &desktop_prepared {
        if prepared
            .proof
            .source_files
            .iter()
            .any(|(path, hash)| expected.get(path) != Some(hash))
        {
            return Err(unsafe_data(
                "desktop authoritative source evidence changed during reconciliation",
            ));
        }
    }
    if terminals
        .hashes
        .iter()
        .any(|(path, hash)| expected.get(path) != Some(hash))
    {
        return Err(unsafe_data(
            "original epoch coordinator evidence changed during conversion",
        ));
    }
    if inventory_only {
        return Ok(BackupOutput::Inventory(attachment_inventory));
    }
    let mut directory = fs::DirBuilder::new();
    let mut output_options = OpenOptions::new();
    output_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        directory.mode(0o700);
        output_options.mode(0o600);
    }
    directory.create(&target)?;
    for (relative, source) in &plan {
        let output = target.join(relative);
        fs::create_dir_all(output.parent().unwrap())?;
        let mut input = file(source)?;
        let mut output = output_options.open(output)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        if hash(&target.join(relative))? != expected[relative]
            || hash(source)? != expected[relative]
        {
            return Err(unsafe_data(format!(
                "copied source changed during export: {relative}"
            )));
        }
    }
    if wal.exists() && file(&wal)?.metadata()?.len() != 0 {
        return Err(unsafe_data("source writer resumed during export"));
    }
    let manifest = CrewBackupManifest {
        version: 1,
        writer_stopped: true,
        source_profile_root: source_profile_root
            .to_str()
            .ok_or_else(|| unsafe_data("non-UTF8 original alias"))?
            .into(),
        owner_subject: principal.subject.clone(),
        project_id: principal.project_id.clone(),
        deployment_id: principal.deployment_id.clone(),
        files: expected,
        chats,
        native_histories,
        attachment_aliases,
        desktop_source: desktop_prepared.map(|prepared| prepared.proof),
        original_epoch_histories: terminals.markers,
    };
    let mut output = output_options.open(target.join("crew-backup.json"))?;
    output.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    output.sync_all()?;
    Ok(BackupOutput::Backup(manifest))
}
