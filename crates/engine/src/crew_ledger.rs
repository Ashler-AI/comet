//! Crew: the durable record of the sandbox commands a desktop sends
//! (docs/crew/zgx-thin-port.md, "Scaffold sandboxes"), Crew's command ledger
//! (`crew_commands` in Ashler-AI/comet `crates/sync/src/store.rs`) in a file
//! of its own beside the profile's store, so upstream's migrations stay
//! upstream's.
//!
//! A command's immutable request is claimed before anything is sent; its
//! outcome, once known, is written once and never changes. A claim with no
//! outcome is ambiguous (the process died, the answer never came): it is
//! reconciled by observation, never sent again.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{Connection, OptionalExtension, params};

/// The ledger's file, in the profile's store directory.
pub(crate) const FILE: &str = "crew-commands.sqlite3";

#[derive(Debug, thiserror::Error)]
pub(crate) enum LedgerError {
    #[error("crew ledger: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("crew ledger: {0}")]
    Io(#[from] std::io::Error),
    /// The id is claimed with another request, or has another outcome.
    #[error("crew command conflict: {0}")]
    Conflict(String),
}

pub(crate) struct Ledger {
    conn: Mutex<Connection>,
}

impl Ledger {
    pub(crate) fn open(dir: &Path) -> Result<Self, LedgerError> {
        std::fs::create_dir_all(dir)?;
        let conn = Connection::open(dir.join(FILE))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS crew_commands (
                command_id TEXT PRIMARY KEY,
                request BLOB NOT NULL,
                outcome BLOB
             ) STRICT;",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claim `id` for `request` before it is sent: `true` for a new claim,
    /// `false` when it was already claimed with this very request (its outcome,
    /// if any, is [`Self::get`]'s). Another request under the same id is a
    /// conflict, never a replacement.
    pub(crate) fn claim(&self, id: &str, request: &[u8]) -> Result<bool, LedgerError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<Vec<u8>> = tx
            .query_row("SELECT request FROM crew_commands WHERE command_id=?1", [id], |row| row.get(0))
            .optional()?;
        if let Some(existing) = existing {
            return if existing == request { Ok(false) } else { Err(LedgerError::Conflict(id.into())) };
        }
        tx.execute("INSERT INTO crew_commands(command_id,request) VALUES (?1,?2)", params![id, request])?;
        tx.commit()?;
        Ok(true)
    }

    /// `id`'s request and outcome.
    pub(crate) fn get(&self, id: &str) -> Result<Option<(Vec<u8>, Option<Vec<u8>>)>, LedgerError> {
        Ok(self
            .conn()
            .query_row("SELECT request,outcome FROM crew_commands WHERE command_id=?1", [id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?)
    }

    /// Write `id`'s outcome. Terminal outcomes are immutable; the same one
    /// again is a no-op.
    pub(crate) fn finish(&self, id: &str, outcome: &[u8]) -> Result<(), LedgerError> {
        let changed = self.conn().execute(
            "UPDATE crew_commands SET outcome=?2 WHERE command_id=?1 AND (outcome IS NULL OR outcome=?2)",
            params![id, outcome],
        )?;
        if changed != 1 {
            return Err(LedgerError::Conflict(format!("{id}: no claim, or another outcome")));
        }
        Ok(())
    }

    /// Claims with no outcome yet, after `after` in id order, at most `limit`
    /// (1–64) of them.
    pub(crate) fn pending(&self, after: &str, limit: usize) -> Result<Vec<(String, Vec<u8>)>, LedgerError> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT command_id,request FROM crew_commands WHERE outcome IS NULL AND command_id>?1 ORDER BY command_id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after, limit.clamp(1, 64) as i64], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Crew's ledger rules: claim before sending, the same request again is
    /// the same claim, another is a conflict; an outcome is written once; and
    /// it all survives the process.
    #[test]
    fn a_command_is_claimed_once_and_settled_once() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path()).unwrap();
        assert!(ledger.claim("create:s1", b"request").unwrap());
        assert!(!ledger.claim("create:s1", b"request").unwrap(), "the same request is the same claim");
        assert!(matches!(ledger.claim("create:s1", b"another"), Err(LedgerError::Conflict(_))));
        assert!(ledger.claim("attach:sbx:e1", b"attach").unwrap());
        assert_eq!(
            ledger.pending("", 64).unwrap(),
            [("attach:sbx:e1".to_owned(), b"attach".to_vec()), ("create:s1".to_owned(), b"request".to_vec())]
        );
        ledger.finish("create:s1", b"confirmed").unwrap();
        ledger.finish("create:s1", b"confirmed").unwrap();
        assert!(matches!(ledger.finish("create:s1", b"rejected"), Err(LedgerError::Conflict(_))));
        assert!(matches!(ledger.finish("missing", b"x"), Err(LedgerError::Conflict(_))));
        assert_eq!(ledger.pending("", 64).unwrap().len(), 1);
        assert_eq!(ledger.pending("attach:sbx:e1", 64).unwrap().len(), 0, "after the cursor");
        drop(ledger);
        let reopened = Ledger::open(dir.path()).unwrap();
        assert_eq!(reopened.get("create:s1").unwrap(), Some((b"request".to_vec(), Some(b"confirmed".to_vec()))));
        assert_eq!(reopened.get("attach:sbx:e1").unwrap(), Some((b"attach".to_vec(), None)));
        assert_eq!(reopened.get("nothing").unwrap(), None);
    }
}
