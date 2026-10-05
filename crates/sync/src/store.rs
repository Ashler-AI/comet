//! `DocsStore` — local SQLite persistence for doc snapshots and the
//! processed-command ledger (ARCHITECTURE §2 command plane: entries are marked
//! processed BEFORE execution so a crash can never double-execute a command).

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use comet_doc::PendingRecord;

/// Errors surfaced by [`DocsStore`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("semantic journal: {0}")]
    SemanticJournal(#[from] serde_json::Error),
    #[error("document scope conflict: {0}")]
    ScopeConflict(String),
}

/// Ordered, append-only migrations. Each entry runs once inside a transaction;
/// `schema_migrations` records what has been applied.
const MIGRATIONS: &[&str] = &[
    // v1 — snapshots + processed-command ledger
    "CREATE TABLE snapshots (
        doc_id   TEXT PRIMARY KEY,
        bytes    BLOB NOT NULL,
        saved_at INTEGER NOT NULL
     ) STRICT;
     CREATE TABLE processed_commands (
        command_id   TEXT PRIMARY KEY,
        processed_at INTEGER NOT NULL
     ) STRICT;",
    // v2 — durable local provenance for commands that do not carry a remote grant.
    "CREATE TABLE trusted_local_commands (
        command_id TEXT PRIMARY KEY,
        created_at INTEGER NOT NULL
     ) STRICT;",
    // v3 — one-shot data migrations scoped by this store's project/principal path.
    "CREATE TABLE local_migrations (
        name TEXT PRIMARY KEY,
        applied_at INTEGER NOT NULL
     ) STRICT;",
    "CREATE TABLE crew_directory_jobs (
        session_id TEXT PRIMARY KEY,
        generation INTEGER NOT NULL DEFAULT 1,
        state TEXT NOT NULL DEFAULT '{}',
        deleted INTEGER NOT NULL DEFAULT 0,
        due_at INTEGER NOT NULL DEFAULT 0,
        retry_at INTEGER NOT NULL DEFAULT 0
     ) STRICT;
     CREATE INDEX crew_directory_jobs_due ON crew_directory_jobs(due_at, session_id);",
    // v5 — completed turns outrank backfill once their retry deadline has passed.
    "ALTER TABLE crew_directory_jobs ADD COLUMN completed_turn_pending INTEGER NOT NULL DEFAULT 0;",
    // v6 — semantic local edits survive authoritative history replacement.
    "CREATE TABLE semantic_intents (
        doc_id TEXT NOT NULL,
        container TEXT NOT NULL,
        record_id TEXT NOT NULL,
        value BLOB NOT NULL,
        acknowledged INTEGER NOT NULL DEFAULT 0 CHECK (acknowledged IN (0, 1)),
        PRIMARY KEY (doc_id, container, record_id)
     ) STRICT;",
    "CREATE TABLE document_scopes (doc_id TEXT PRIMARY KEY, scope TEXT NOT NULL) STRICT;",
    // v8 — accepted device wakes are independent of replaceable CRDT caches.
    "CREATE TABLE pending_nudges (
        chat_id TEXT PRIMARY KEY,
        nudge_id TEXT NOT NULL,
        accepted_at INTEGER NOT NULL
     ) STRICT;",
    // v9 — zeroblob stays compact only at the record tail, after saved_at.
    // Keep rowids and every cache/backup byte; all callers name their columns.
    "CREATE TABLE snapshots_blob_tail (
        doc_id TEXT PRIMARY KEY,
        saved_at INTEGER NOT NULL,
        bytes BLOB NOT NULL
     ) STRICT;
     INSERT INTO snapshots_blob_tail (rowid, doc_id, saved_at, bytes)
        SELECT rowid, doc_id, saved_at, bytes FROM snapshots;
     DROP TABLE snapshots;
     ALTER TABLE snapshots_blob_tail RENAME TO snapshots;",
];

/// SQLite-backed store under a data directory (`{data_dir}/docs.sqlite3`).
///
/// Holds warm-open doc snapshots (the DO room is authoritative; these make
/// cold starts instant and offline restarts possible) and the command ledger
/// that gives command execution mark-BEFORE-execute idempotence.
pub struct DocsStore {
    conn: Mutex<Connection>,
    record_buffer: Mutex<Vec<u8>>,
    directory_changed: tokio::sync::watch::Sender<u64>,
}

impl DocsStore {
    /// Open (creating directory, database, and schema as needed).
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let data_dir = data_dir.as_ref();
        std::fs::create_dir_all(data_dir)?;
        let mut conn = Connection::open(data_dir.join("docs.sqlite3"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Self {
            directory_changed: tokio::sync::watch::channel(0).0,
            conn: Mutex::new(conn),
            record_buffer: Mutex::new(Vec::new()),
        })
    }

    /// Commit before acknowledging a device-room nudge. Duplicate delivery never
    /// renews its deadline; a newly accepted token replaces only this chat's wake.
    pub fn accept_nudge(&self, chat_id: &str, nudge_id: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO pending_nudges(chat_id, nudge_id, accepted_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(chat_id) DO UPDATE SET nudge_id = excluded.nudge_id,
             accepted_at = excluded.accepted_at WHERE pending_nudges.nudge_id != excluded.nudge_id",
            params![chat_id, nudge_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn pending_nudges(&self) -> Result<Vec<(String, String, i64)>, StoreError> {
        let conn = self.conn();
        let mut query = conn.prepare("SELECT chat_id, nudge_id, accepted_at FROM pending_nudges ORDER BY chat_id")?;
        let rows = query.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// An older processor must never remove a newer accepted wake.
    pub fn complete_nudge(&self, chat_id: &str, nudge_id: &str) -> Result<bool, StoreError> {
        Ok(self.conn().execute(
            "DELETE FROM pending_nudges WHERE chat_id = ?1 AND nudge_id = ?2",
            params![chat_id, nudge_id],
        )? != 0)
    }

    /// Latest saved snapshot for `doc_id`, if any.
    pub fn load_snapshot(&self, doc_id: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let bytes = self
            .conn()
            .query_row(
                "SELECT bytes FROM snapshots WHERE doc_id = ?1",
                params![doc_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(bytes)
    }

    /// Save (upsert) the snapshot for `doc_id`.
    pub fn save_snapshot(&self, doc_id: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.save_snapshot_with_priority(doc_id, bytes, false)
    }

    /// Persist a completed turn and prioritize its next directory claim atomically.
    pub fn save_completed_snapshot(&self, doc_id: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.save_snapshot_with_priority(doc_id, bytes, true)
    }

    fn save_snapshot_with_priority(
        &self,
        doc_id: &str,
        bytes: &[u8],
        completed: bool,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        write_snapshot(&tx, doc_id, bytes)?;
        // Only global room-shaped ids enter discovery; the engine verifies UUID
        // and authenticated ownership before reading or transmitting their data.
        if doc_id.len() == 36 {
            tx.execute(
                "INSERT INTO crew_directory_jobs(session_id, due_at, completed_turn_pending) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET generation = generation + 1,
                 due_at = MAX(retry_at, MIN(due_at, excluded.due_at)),
                 completed_turn_pending = MAX(completed_turn_pending, excluded.completed_turn_pending) WHERE deleted = 0",
                params![doc_id, if completed { 0 } else { now_ms() + 30_000 }, completed],
            )?;
        }
        tx.commit()?;
        self.directory_changed
            .send_modify(|value| *value = value.wrapping_add(1));
        Ok(())
    }

    /// Only unacknowledged semantic edits are eligible for history reconciliation.
    /// Acknowledged outcomes remain durable so a restart cannot resurrect them.
    pub fn load_pending_records(&self, doc_id: &str) -> Result<Vec<PendingRecord>, StoreError> {
        let conn = self.conn();
        let mut query = conn.prepare(
            "SELECT value FROM semantic_intents WHERE doc_id = ?1 AND acknowledged = 0
             ORDER BY container, record_id",
        )?;
        let rows = query.query_map(params![doc_id], |row| row.get::<_, Vec<u8>>(0))?;
        rows.map(|row| Ok(serde_json::from_slice(&row?)?)).collect()
    }

    pub fn load_command_outcome(&self, doc_id: &str, command_id: &str) -> Result<Option<comet_doc::SessionCommandEntry>, StoreError> {
        let bytes: Option<Vec<u8>> = self.conn().query_row(
            "SELECT value FROM semantic_intents WHERE doc_id = ?1 AND container = 'commands' AND record_id = ?2",
            params![doc_id,command_id], |row| row.get(0),
        ).optional()?;
        let Some(bytes) = bytes else { return Ok(None) };
        let record: PendingRecord = serde_json::from_slice(&bytes)?;
        record.value.map(serde_json::from_value).transpose().map_err(StoreError::from)
    }

    /// Persist intent outcomes and an optional authoritative snapshot in one
    /// transaction. Adoption is not a new local edit or a directory generation.
    pub fn save_pending_records(
        &self,
        doc_id: &str,
        records: &[PendingRecord],
        snapshot: Option<&[u8]>,
    ) -> Result<(), StoreError> {
        self.save_pending_records_for_snapshot(doc_id,doc_id,records,snapshot)
    }

    pub fn save_pending_records_for_snapshot<'a>(
        &self, doc_id: &str, snapshot_id: &str, records: impl IntoIterator<Item = &'a PendingRecord>, snapshot: Option<&[u8]>,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let scope: Option<String> = tx.query_row("SELECT scope FROM document_scopes WHERE doc_id = ?1",params![snapshot_id],|row| row.get(0)).optional()?;
        if scope.as_deref().is_some_and(|scope| scope != doc_id) {
            return Err(StoreError::ScopeConflict("retired document binding cannot write a different room scope".into()));
        }
        upsert_pending_records(&tx, doc_id, records, &mut self.record_buffer.lock().unwrap_or_else(PoisonError::into_inner))?;
        tx.execute(
            "INSERT OR IGNORE INTO local_migrations (name, applied_at) VALUES (?1, ?2)",
            params![format!("semantic/{doc_id}"), now_ms()],
        )?;
        if let Some(snapshot) = snapshot {
            write_snapshot(&tx, snapshot_id, snapshot)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn has_semantic_journal(&self, doc_id: &str) -> Result<bool, StoreError> {
        self.is_local_migration_applied(&format!("semantic/{doc_id}"))
    }

    pub fn has_semantic_records(&self, doc_id: &str) -> Result<bool, StoreError> {
        Ok(self.conn().query_row("SELECT 1 FROM semantic_intents WHERE doc_id = ?1 LIMIT 1",params![doc_id],|_| Ok(())).optional()?.is_some())
    }

    /// Bootstrap legacy local intent once, without dropping snapshots, processed
    /// commands, admission receipts, or already acknowledged semantic records.
    pub fn migrate_pending_records(&self, doc_id: &str, records: &[PendingRecord]) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "INSERT OR IGNORE INTO local_migrations (name, applied_at) VALUES (?1, ?2)",
            params![format!("semantic/{doc_id}"), now_ms()],
        )?;
        if changed != 0 {
            upsert_pending_records(&tx, doc_id, records, &mut self.record_buffer.lock().unwrap_or_else(PoisonError::into_inner))?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn document_scope(&self, doc_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self.conn().query_row("SELECT scope FROM document_scopes WHERE doc_id = ?1",params![doc_id],|row| row.get(0)).optional()?)
    }

    /// A UUID cache cannot cross authenticated room scopes. The sole allowed
    /// retarget is an explicitly verified empty pre-attach document.
    pub fn claim_document_scope(&self, doc_id: &str, scope: &str, empty_previous: Option<&str>) -> Result<(), StoreError> {
        let mut conn = self.conn(); let tx = conn.transaction()?;
        let previous: Option<String> = tx.query_row("SELECT scope FROM document_scopes WHERE doc_id = ?1",params![doc_id],|row| row.get(0)).optional()?;
        match previous {
            Some(previous) if previous == scope => {}
            Some(previous) if empty_previous == Some(previous.as_str()) => {
                tx.execute("UPDATE document_scopes SET scope = ?2 WHERE doc_id = ?1",params![doc_id,scope])?;
            }
            Some(_) => return Err(StoreError::ScopeConflict(format!("{doc_id}: original snapshot and intents retained"))),
            None => { tx.execute("INSERT INTO document_scopes (doc_id,scope) VALUES (?1,?2)",params![doc_id,scope])?; }
        }
        tx.commit()?; Ok(())
    }

    /// Delete the snapshot row for `doc_id` (destructive schema breaks: the
    /// legacy `workspace` row is dropped on open). Missing rows are a no-op.
    pub fn delete_snapshot(&self, doc_id: &str) -> Result<(), StoreError> {
        self.conn()
            .execute("DELETE FROM snapshots WHERE doc_id = ?1", params![doc_id])?;
        Ok(())
    }

    /// Whether `command_id` has already been claimed for execution.
    pub fn is_processed(&self, command_id: &str) -> Result<bool, StoreError> {
        let hit = self
            .conn()
            .query_row(
                "SELECT 1 FROM processed_commands WHERE command_id = ?1",
                params![command_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// Claim `command_id` for execution — call BEFORE executing (ledger rule:
    /// a crash mid-execution must never re-run the command). Returns `true`
    /// if this call claimed it, `false` if it was already processed.
    pub fn mark_processed(&self, command_id: &str) -> Result<bool, StoreError> {
        let changed = self.conn().execute(
            "INSERT OR IGNORE INTO processed_commands (command_id, processed_at) VALUES (?1, ?2)",
            params![command_id, now_ms()],
        )?;
        Ok(changed > 0)
    }

    /// Persist that this command was authored through this device's local API.
    /// Remote CRDT peers cannot create rows in this device-local database.
    pub fn trust_local_command(&self, command_id: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT OR IGNORE INTO trusted_local_commands (command_id, created_at) VALUES (?1, ?2)",
            params![command_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn is_trusted_local_command(&self, command_id: &str) -> Result<bool, StoreError> {
        let hit = self
            .conn()
            .query_row(
                "SELECT 1 FROM trusted_local_commands WHERE command_id = ?1",
                params![command_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// Bound durable admission receipts without touching execution deduplication.
    pub fn prune_local_commands(&self, created_before: i64) -> Result<(), StoreError> {
        self.conn().execute(
            "DELETE FROM trusted_local_commands WHERE created_at < ?1",
            params![created_before],
        )?;
        Ok(())
    }

    pub fn forget_local_command(&self, command_id: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "DELETE FROM trusted_local_commands WHERE command_id = ?1",
            params![command_id],
        )?;
        Ok(())
    }

    /// Whether an identity-local data migration has completed.
    pub fn is_local_migration_applied(&self, name: &str) -> Result<bool, StoreError> {
        let hit = self
            .conn()
            .query_row(
                "SELECT 1 FROM local_migrations WHERE name = ?1",
                params![name],
                |_| Ok(()),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// Mark an identity-local data migration complete after its durable state is saved.
    pub fn mark_local_migration_applied(&self, name: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT OR IGNORE INTO local_migrations (name, applied_at) VALUES (?1, ?2)",
            params![name, now_ms()],
        )?;
        Ok(())
    }

    /// Keyset inventory: never reopen rooms or retain all snapshots for backfill.
    pub fn snapshot_ids_after(&self, after: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        let mut query = conn
            .prepare("SELECT doc_id FROM snapshots WHERE doc_id > ?1 ORDER BY doc_id LIMIT 32")?;
        Ok(query
            .query_map(params![after], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn load_bounded_snapshot(
        &self,
        id: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let conn = self.conn();
        let size: Option<usize> = conn
            .query_row(
                "SELECT length(bytes) FROM snapshots WHERE doc_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        if size.is_some_and(|size| size > max_bytes) {
            return Err(
                std::io::Error::other("session snapshot exceeds directory scan budget").into(),
            );
        }
        conn.query_row(
            "SELECT bytes FROM snapshots WHERE doc_id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn queue_directory(&self, id: &str, deleted: bool) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO crew_directory_jobs(session_id, deleted) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET generation = generation + 1,
             deleted = MAX(deleted, excluded.deleted), due_at = retry_at",
            params![id, deleted],
        )?;
        self.directory_changed
            .send_modify(|value| *value = value.wrapping_add(1));
        Ok(())
    }

    pub fn claim_directory(&self) -> Result<Option<(String, i64, String, bool)>, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let job = tx.query_row(
            "SELECT session_id, generation, state, deleted FROM crew_directory_jobs WHERE due_at <= ?1 ORDER BY completed_turn_pending DESC, due_at, session_id LIMIT 1",
            params![now_ms()], |row| Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional()?;
        if let Some((id, _, _, _)) = &job {
            // Consume only this claim's priority; a completion arriving in flight sets it again.
            tx.execute("UPDATE crew_directory_jobs SET due_at = ?2, completed_turn_pending = 0 WHERE session_id = ?1", params![id, now_ms() + 120_000])?;
        }
        tx.commit()?;
        Ok(job)
    }

    pub fn save_directory_state(
        &self,
        id: &str,
        state: &str,
        deleted: bool,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE crew_directory_jobs SET state = ?2 WHERE session_id = ?1 AND deleted = ?3",
            params![id, state, deleted],
        )?;
        Ok(())
    }

    pub fn queue_directory_deletion(&self, id: &str, state: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO crew_directory_jobs(session_id, deleted, state) VALUES (?1, 1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET generation = generation + 1, deleted = 1, due_at = 0, state = excluded.state",
            params![id, state],
        )?;
        self.directory_changed
            .send_modify(|value| *value = value.wrapping_add(1));
        Ok(())
    }

    pub fn watch_directory(&self) -> tokio::sync::watch::Receiver<u64> {
        self.directory_changed.subscribe()
    }

    pub fn directory_retry_delay(&self) -> Result<Option<std::time::Duration>, StoreError> {
        let due: Option<i64> = self.conn().query_row(
            "SELECT MIN(due_at) FROM crew_directory_jobs WHERE due_at < 9223372036854775807",
            [],
            |row| row.get(0),
        )?;
        Ok(due
            .map(|at| std::time::Duration::from_millis(at.saturating_sub(now_ms()).max(0) as u64)))
    }

    pub fn settle_directory(
        &self,
        id: &str,
        generation: i64,
        success: bool,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE crew_directory_jobs SET
             due_at = CASE WHEN ?3 THEN CASE WHEN generation = ?2 THEN 9223372036854775807 ELSE due_at END ELSE ?4 END,
             retry_at = CASE WHEN ?3 THEN 0 ELSE ?4 END WHERE session_id = ?1",
            params![id, generation, success, now_ms() + 30_000],
        )?;
        Ok(())
    }

    pub fn defer_directory_title(
        &self,
        id: &str,
        generation: i64,
        deadline: i64,
    ) -> Result<(), StoreError> {
        self.conn().execute("UPDATE crew_directory_jobs SET due_at = ?3 WHERE session_id = ?1 AND generation = ?2 AND deleted = 0", params![id, generation, deadline])?;
        Ok(())
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock only means another thread panicked mid-query; the
        // connection itself is still usable.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn write_snapshot(tx: &rusqlite::Transaction<'_>, doc_id: &str, bytes: &[u8]) -> Result<(), StoreError> {
    use std::io::Write;
    let length = i32::try_from(bytes.len())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let row_id: i64 = tx.query_row(
        "INSERT INTO snapshots (doc_id, bytes, saved_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(doc_id) DO UPDATE SET bytes = excluded.bytes, saved_at = excluded.saved_at
         RETURNING rowid",
        params![doc_id, rusqlite::blob::ZeroBlob(length), now_ms()],
        |row| row.get(0),
    )?;
    if !bytes.is_empty() {
        let mut blob = tx.blob_open(rusqlite::DatabaseName::Main, "snapshots", "bytes", row_id, false)?;
        blob.write_all(bytes)?;
        blob.close()?;
    }
    Ok(())
}

fn upsert_pending_records<'a>(
    tx: &rusqlite::Transaction<'_>,
    doc_id: &str,
    records: impl IntoIterator<Item = &'a PendingRecord>,
    buffer: &mut Vec<u8>,
) -> Result<(), StoreError> {
    let mut upsert = tx.prepare(
        "INSERT INTO semantic_intents (doc_id, container, record_id, value, acknowledged)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(doc_id, container, record_id) DO UPDATE SET
         value = excluded.value, acknowledged = excluded.acknowledged",
    )?;
    for record in records {
        buffer.clear();
        serde_json::to_writer(&mut *buffer, record)?;
        upsert.execute(params![doc_id, record.container, record.key, buffer.as_slice(), record.acknowledged])?;
    }
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
         ) STRICT",
    )?;
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, now_ms()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(key: &str) -> PendingRecord {
        PendingRecord {
            container: "chats".into(),
            key: key.into(),
            before: Some(serde_json::json!({ "title": "before" })),
            value: Some(serde_json::json!({ "title": "after" })),
            version: vec![1, 2, 3],
            acknowledged: false,
        }
    }

    #[test]
    fn durable_nudge_survives_cache_replacement_restart_and_old_completion() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        store.accept_nudge("chat-a", "old-token").unwrap();
        let original = store.pending_nudges().unwrap();
        store.conn().execute("UPDATE pending_nudges SET accepted_at = 7 WHERE chat_id = 'chat-a'", []).unwrap();
        store.accept_nudge("chat-a", "old-token").unwrap();
        assert_eq!(store.pending_nudges().unwrap(), vec![("chat-a".into(), "old-token".into(), 7)]);
        store.accept_nudge("chat-a", "new-token").unwrap();
        store.accept_nudge("chat-b", "other-token").unwrap();
        store.save_snapshot("chat-a", b"replacement-cache").unwrap();
        store.save_pending_records("chat-a", &[], None).unwrap();
        assert!(!store.complete_nudge("chat-a", &original[0].1).unwrap());
        let pending = store.pending_nudges().unwrap();
        drop(store);
        let store = DocsStore::open(dir.path()).unwrap();
        assert_eq!(store.pending_nudges().unwrap(), pending);
        assert!(store.complete_nudge("chat-a", "new-token").unwrap());
        assert_eq!(store.pending_nudges().unwrap(), vec![pending[1].clone()]);
    }

    #[test]
    fn semantic_records_survive_restart_with_deletes_and_document_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let change = pending("change");
        let mut deletion = pending("deleted");
        deletion.value = None;
        let records = [change.clone(), deletion];
        store.migrate_pending_records("workspace", &records).unwrap();
        store.save_pending_records("other-workspace", &[pending("change")], None).unwrap();
        store.mark_processed("executed-command").unwrap();
        store.trust_local_command("admitted-command").unwrap();
        drop(store);

        let store = DocsStore::open(dir.path()).unwrap();
        assert!(store.has_semantic_journal("workspace").unwrap());
        assert_eq!(serde_json::to_value(store.load_pending_records("workspace").unwrap()).unwrap(), serde_json::to_value(&records).unwrap());
        assert_eq!(serde_json::to_value(store.load_pending_records("other-workspace").unwrap()).unwrap(), serde_json::to_value([change]).unwrap());
        assert!(store.is_processed("executed-command").unwrap());
        assert!(store.is_trusted_local_command("admitted-command").unwrap());
    }

    #[test]
    fn semantic_adoption_commits_snapshot_and_acknowledgements_together() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let record = pending("chat-a");
        store.save_snapshot("workspace", b"old-history").unwrap();
        store.migrate_pending_records("workspace", &[record.clone()]).unwrap();
        let mut acknowledged = record.clone();
        acknowledged.acknowledged = true;
        store.conn().execute_batch(
            "CREATE TRIGGER fail_adoption BEFORE UPDATE ON snapshots
             BEGIN SELECT RAISE(ABORT, 'injected snapshot write failure'); END;",
        ).unwrap();
        assert!(store.save_pending_records("workspace", &[acknowledged.clone()], Some(b"authoritative-history")).is_err());
        assert_eq!(store.load_snapshot("workspace").unwrap().as_deref(), Some(&b"old-history"[..]));
        assert_eq!(serde_json::to_value(store.load_pending_records("workspace").unwrap()).unwrap(), serde_json::to_value([record.clone()]).unwrap());
        store.conn().execute_batch("DROP TRIGGER fail_adoption").unwrap();
        store.save_pending_records("workspace", &[acknowledged.clone()], Some(b"authoritative-history")).unwrap();
        drop(store);

        let store = DocsStore::open(dir.path()).unwrap();
        assert_eq!(store.load_snapshot("workspace").unwrap().as_deref(), Some(&b"authoritative-history"[..]));
        assert!(store.load_pending_records("workspace").unwrap().is_empty());
        // Re-running legacy bootstrap cannot revive an acknowledged instruction.
        store.migrate_pending_records("workspace", &[record]).unwrap();
        assert!(store.load_pending_records("workspace").unwrap().is_empty());
        let retained: Vec<u8> = store.conn().query_row(
            "SELECT value FROM semantic_intents WHERE doc_id = 'workspace' AND container = 'chats' AND record_id = 'chat-a' AND acknowledged = 1",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&retained).unwrap(), serde_json::to_value(acknowledged).unwrap());
    }

    #[test]
    fn failed_semantic_migration_rolls_back_marker_and_all_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let records = [pending("first"), pending("reject")];
        store.conn().execute_batch(
            "CREATE TRIGGER fail_intent BEFORE INSERT ON semantic_intents
             WHEN NEW.record_id = 'reject'
             BEGIN SELECT RAISE(ABORT, 'injected intent write failure'); END;",
        ).unwrap();
        assert!(store.migrate_pending_records("workspace", &records).is_err());
        assert!(!store.has_semantic_journal("workspace").unwrap());
        assert!(store.load_pending_records("workspace").unwrap().is_empty());
        store.conn().execute_batch("DROP TRIGGER fail_intent").unwrap();
        store.migrate_pending_records("workspace", &records).unwrap();
        assert!(store.has_semantic_journal("workspace").unwrap());
        assert_eq!(serde_json::to_value(store.load_pending_records("workspace").unwrap()).unwrap(), serde_json::to_value(records).unwrap());
    }

    #[test]
    fn directory_snapshots_coalesce_without_postponing_deadline_or_bypassing_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let id = "10000000-0000-4000-8000-000000000001";
        store.save_snapshot(id, b"first").unwrap();
        assert!(store.claim_directory().unwrap().is_none());
        let first_due: i64 = store
            .conn()
            .query_row(
                "SELECT due_at FROM crew_directory_jobs WHERE session_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        store.save_snapshot(id, b"newest").unwrap();
        let next_due: i64 = store
            .conn()
            .query_row(
                "SELECT due_at FROM crew_directory_jobs WHERE session_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(next_due, first_due);
        store.queue_directory(id, false).unwrap();
        let (_, generation, _, _) = store.claim_directory().unwrap().unwrap();
        store.save_snapshot(id, b"raced").unwrap();
        store.settle_directory(id, generation, false).unwrap();
        store.save_snapshot(id, b"later").unwrap();
        store.queue_directory(id, false).unwrap();
        assert!(store.claim_directory().unwrap().is_none());
    }

    #[test]
    fn completed_snapshot_prioritizes_failed_job_durably_without_resetting_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let id = "90000000-0000-4000-8000-000000000001";
        let backlog = "10000000-0000-4000-8000-000000000001";
        store.queue_directory(id, false).unwrap();
        let (_, generation, _, _) = store.claim_directory().unwrap().unwrap();
        store.settle_directory(id, generation, false).unwrap();
        let state = r#"{"nextTitleAt":9223372036854775807}"#;
        store.save_directory_state(id, state, false).unwrap();
        store.save_completed_snapshot(id, b"completed").unwrap();
        assert!(store.claim_directory().unwrap().is_none());
        // Advance past transport backoff without sleeping or resetting inference state.
        store
            .conn()
            .execute(
                "UPDATE crew_directory_jobs SET due_at = 1, retry_at = 1 WHERE session_id = ?1",
                params![id],
            )
            .unwrap();
        store.queue_directory(backlog, false).unwrap();
        drop(store);

        let store = DocsStore::open(dir.path()).unwrap();
        // Restart backfill and later debounced saves must not demote a completion.
        store.queue_directory(id, false).unwrap();
        store.save_snapshot(id, b"latest").unwrap();
        let (claimed, _, saved, _) = store.claim_directory().unwrap().unwrap();
        assert_eq!(claimed, id);
        assert_eq!(saved, state);
        assert_eq!(
            store.load_snapshot(id).unwrap().as_deref(),
            Some(&b"latest"[..])
        );
    }

    #[test]
    fn completed_priority_is_consumed_before_retry_or_title_deferral() {
        for deferred in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = DocsStore::open(dir.path()).unwrap();
            let id = "90000000-0000-4000-8000-000000000001";
            let backlog = "10000000-0000-4000-8000-000000000001";
            store.save_completed_snapshot(id, b"completed").unwrap();
            let (_, generation, _, _) = store.claim_directory().unwrap().unwrap();
            if deferred {
                store
                    .defer_directory_title(id, generation, i64::MAX - 1)
                    .unwrap();
            } else {
                store.settle_directory(id, generation, false).unwrap();
            }
            assert!(store.claim_directory().unwrap().is_none());
            // Once due again, the failed/deferred completion no longer jumps the backlog.
            store
                .conn()
                .execute(
                    "UPDATE crew_directory_jobs SET due_at = 1, retry_at = 1 WHERE session_id = ?1",
                    params![id],
                )
                .unwrap();
            store.queue_directory(backlog, false).unwrap();
            let (claimed, generation, _, _) = store.claim_directory().unwrap().unwrap();
            assert_eq!(claimed, backlog);
            store.settle_directory(backlog, generation, true).unwrap();
            assert_eq!(store.claim_directory().unwrap().unwrap().0, id);
        }
    }

    #[test]
    fn completion_during_claim_survives_success_failure_and_title_deferral() {
        for outcome in ["success", "failure", "deferred"] {
            let dir = tempfile::tempdir().unwrap();
            let store = DocsStore::open(dir.path()).unwrap();
            let id = "90000000-0000-4000-8000-000000000001";
            let backlog = "10000000-0000-4000-8000-000000000001";
            store.save_completed_snapshot(id, b"first").unwrap();
            let (_, generation, _, _) = store.claim_directory().unwrap().unwrap();
            store
                .save_completed_snapshot(id, b"new completion")
                .unwrap();
            match outcome {
                "deferred" => store
                    .defer_directory_title(id, generation, i64::MAX - 1)
                    .unwrap(),
                _ => store
                    .settle_directory(id, generation, outcome == "success")
                    .unwrap(),
            }
            if outcome == "failure" {
                assert!(store.claim_directory().unwrap().is_none());
                store.conn().execute("UPDATE crew_directory_jobs SET due_at = 1, retry_at = 1 WHERE session_id = ?1", params![id]).unwrap();
            }
            store.queue_directory(backlog, false).unwrap();
            let (claimed, newer, _, _) = store.claim_directory().unwrap().unwrap();
            assert_eq!(claimed, id, "{outcome}");
            assert!(newer > generation);
            assert_eq!(
                store.load_snapshot(id).unwrap().as_deref(),
                Some(&b"new completion"[..])
            );
        }
    }

    #[test]
    fn directory_acknowledgement_cannot_drop_newer_work_or_resurrect_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let id = "10000000-0000-4000-8000-000000000001";
        let store = DocsStore::open(dir.path()).unwrap();
        store.queue_directory(id, false).unwrap();
        let (_, generation, _, _) = store.claim_directory().unwrap().unwrap();
        store.queue_directory(id, false).unwrap();
        store.settle_directory(id, generation, true).unwrap();
        let (_, newer, _, _) = store.claim_directory().unwrap().unwrap();
        assert!(newer > generation);
        store
            .queue_directory_deletion(
                id,
                r#"{"sourceVersion":{"s:1":4},"deploymentId":"staging"}"#,
            )
            .unwrap();
        store.save_directory_state(id, "{}", false).unwrap();
        drop(store);
        let restarted = DocsStore::open(dir.path()).unwrap();
        restarted.queue_directory(id, false).unwrap();
        let (_, _, state, deleted) = restarted.claim_directory().unwrap().unwrap();
        assert!(deleted);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&state).unwrap()["deploymentId"],
            "staging"
        );
    }

    #[test]
    fn snapshot_roundtrip_and_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        assert_eq!(store.load_snapshot("chat-1").unwrap(), None);
        store.save_snapshot("chat-1", b"initial").unwrap();
        let pages: Vec<u8> = (0..11 * 4096 + 3).map(|index| index as u8).collect();
        for bytes in [pages.as_slice(), &[0, 255], &[]] {
            store.save_snapshot("chat-1", bytes).unwrap();
            assert_eq!(store.load_snapshot("chat-1").unwrap().as_deref(), Some(bytes));
        }
        store.save_snapshot("chat-2", b"other").unwrap();
        assert_eq!(store.load_snapshot("chat-1").unwrap().as_deref(), Some(&[][..]));
    }

    #[test]
    fn processed_ledger_claims_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();

        assert!(!store.is_processed("cmd-1").unwrap());
        assert!(store.mark_processed("cmd-1").unwrap(), "first mark claims");
        assert!(store.is_processed("cmd-1").unwrap());
        assert!(
            !store.mark_processed("cmd-1").unwrap(),
            "second mark must not re-claim"
        );
    }

    #[test]
    fn local_command_trust_is_durable_and_revocable() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();

        assert!(!store.is_trusted_local_command("cmd-local").unwrap());
        store.trust_local_command("cmd-local").unwrap();
        assert!(store.is_trusted_local_command("cmd-local").unwrap());
        drop(store);

        let store = DocsStore::open(dir.path()).unwrap();
        assert!(store.is_trusted_local_command("cmd-local").unwrap());
        store.forget_local_command("cmd-local").unwrap();
        assert!(!store.is_trusted_local_command("cmd-local").unwrap());
    }

    #[test]
    fn local_command_retention_preserves_original_admission_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        store.trust_local_command("old").unwrap();
        store
            .conn()
            .execute(
                "UPDATE trusted_local_commands SET created_at = 10 WHERE command_id = 'old'",
                [],
            )
            .unwrap();
        store.mark_processed("old").unwrap();
        store.trust_local_command("old").unwrap();
        store.trust_local_command("current").unwrap();
        store.prune_local_commands(11).unwrap();
        assert!(
            !store.is_trusted_local_command("old").unwrap(),
            "retry must not refresh original admission time"
        );
        assert!(store.is_trusted_local_command("current").unwrap());
        assert!(
            store.is_processed("old").unwrap(),
            "retention must not allow re-execution"
        );
    }

    #[test]
    fn local_migration_markers_are_durable_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        assert!(!store.is_local_migration_applied("membership-v1").unwrap());
        store.mark_local_migration_applied("membership-v1").unwrap();
        store.mark_local_migration_applied("membership-v1").unwrap();
        drop(store);

        let store = DocsStore::open(dir.path()).unwrap();
        assert!(store.is_local_migration_applied("membership-v1").unwrap());
    }

    #[test]
    fn reopen_preserves_data_and_migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = DocsStore::open(dir.path()).unwrap();
            store.save_snapshot("chat-1", b"persisted").unwrap();
            store.mark_processed("cmd-1").unwrap();
        }
        let store = DocsStore::open(dir.path()).unwrap(); // re-runs migrate()
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"persisted"[..])
        );
        assert!(store.is_processed("cmd-1").unwrap());
        assert!(!store.mark_processed("cmd-1").unwrap());
    }

    #[test]
    fn legacy_snapshot_migration_preserves_bytes_clocks_rowids_and_command_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let payload: Vec<u8> = (0..11 * 4096 + 3).map(|index| index as u8).collect();
        {
            let conn = Connection::open(dir.path().join("docs.sqlite3")).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL) STRICT;
                INSERT INTO schema_migrations VALUES (1, 1);
                INSERT INTO processed_commands VALUES ('accepted-command', 17);").unwrap();
            conn.execute("INSERT INTO snapshots (rowid,doc_id,bytes,saved_at) VALUES (?1,?2,?3,?4)",
                params![41,"public-room",payload.as_slice(),1234567890_i64]).unwrap();
            conn.execute("INSERT INTO snapshots (rowid,doc_id,bytes,saved_at) VALUES (?1,?2,?3,?4)",
                params![97,"recovery-original/room",&[] as &[u8],0]).unwrap();
        }
        for _ in 0..2 {
            let store = DocsStore::open(dir.path()).unwrap();
            assert_eq!(store.load_snapshot("public-room").unwrap(),Some(payload.clone()));
            assert_eq!(store.load_snapshot("recovery-original/room").unwrap(),Some(Vec::new()));
            let metadata: Vec<(String,i64,i64)> = store.conn().prepare("SELECT doc_id,rowid,saved_at FROM snapshots ORDER BY rowid")
                .unwrap().query_map([],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap().map(Result::unwrap).collect();
            assert_eq!(metadata,[("public-room".into(),41,1234567890),("recovery-original/room".into(),97,0)]);
            assert!(store.is_processed("accepted-command").unwrap());
        }
        let store = DocsStore::open(dir.path()).unwrap();
        store.save_snapshot("public-room",b"new checkpoint").unwrap();
        assert_eq!(store.load_snapshot("public-room").unwrap().as_deref(),Some(&b"new checkpoint"[..]));
        assert_eq!(store.load_snapshot("recovery-original/room").unwrap(),Some(Vec::new()));
    }
}
