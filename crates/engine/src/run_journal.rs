//! Per-session on-disk event journal (port of comet's `run-journal.ts`, JSONL-shaped).
//!
//! One append-only JSONL file per chat under `{data_dir}/journals/{chat_id}.jsonl`; each
//! line is `{"seq": n, "event": AgentEvent}` with a monotonically increasing `seq`. The
//! journal is the durable replay source for live streams (`Subscribe` = replay then tail
//! the broadcast hub) and the crash-recovery gauge: a journal whose LAST event is not
//! `Done` belongs to a run that died mid-stream — boot recovery stamps its doc entry
//! `aborted` and closes the journal with a synthetic `Done`.
//!
//! Bounded-window compaction is deferred (whole file kept for now, per M2 scope); a torn
//! trailing line from a crash mid-write is tolerated everywhere.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use comet_proto::AgentEvent;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalLine<E = AgentEvent> {
    seq: u64,
    event: E,
}

struct ChatJournal {
    file: BufWriter<File>,
    next_seq: u64,
    /// True when the file ends without a newline (torn write) — the next append
    /// starts with one so the torn line stays isolated.
    needs_newline: bool,
    harness_session: HarnessSessionMetadata,
}

#[derive(Default)]
struct HarnessSessionMetadata {
    cwd: String,
    last: Option<(String, String)>,
}

impl HarnessSessionMetadata {
    fn observe(&mut self, event: &AgentEvent) {
        let session_id = match event {
            AgentEvent::SessionStarted { session_id, cwd, .. } => {
                self.cwd.clone_from(cwd);
                session_id
            }
            AgentEvent::Done { session_id: Some(session_id), .. } => session_id,
            _ => return,
        };
        if !session_id.is_empty() {
            let last = self.last.get_or_insert_with(|| (String::new(), String::new()));
            last.0.clone_from(session_id);
            last.1.clone_from(&self.cwd);
        }
    }
}

/// Append-only JSONL journal store, one file per chat.
pub struct RunJournal {
    dir: PathBuf,
    open_files: Mutex<HashMap<String, ChatJournal>>,
}

impl RunJournal {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, JournalError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            open_files: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, ChatJournal>> {
        self.open_files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn path_for(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{}.jsonl", sanitize_id(chat_id)))
    }

    fn attempts_path(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{}.resume", sanitize_id(chat_id)))
    }

    pub(crate) fn save_context<T: Serialize>(
        &self,
        chat_id: &str,
        context: &T,
    ) -> Result<(), JournalError> {
        self.save_record(chat_id, "context", context)
    }

    fn save_record<T: Serialize>(
        &self,
        chat_id: &str,
        extension: &str,
        context: &T,
    ) -> Result<(), JournalError> {
        let _guard = self.lock();
        let path = self
            .dir
            .join(format!("{}.{}", sanitize_id(chat_id), extension));
        let temporary = path.with_extension(format!("{extension}.tmp"));
        let bytes = serde_json::to_vec(&(chat_id, context))?;
        let mut file = File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(temporary, path)?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    pub(crate) fn read_context<T: serde::de::DeserializeOwned>(
        &self,
        chat_id: &str,
    ) -> Result<Option<T>, JournalError> {
        self.read_record(chat_id, "context")
    }

    fn read_record<T: serde::de::DeserializeOwned>(
        &self,
        chat_id: &str,
        extension: &str,
    ) -> Result<Option<T>, JournalError> {
        let path = self
            .dir
            .join(format!("{}.{}", sanitize_id(chat_id), extension));
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let (stored_id, context): (String, T) = serde_json::from_slice(&bytes)?;
        if stored_id != chat_id {
            return Err(std::io::Error::other("session_context_binding_mismatch").into());
        }
        Ok(Some(context))
    }

    pub(crate) fn save_recovery<T: Serialize>(
        &self,
        chat_id: &str,
        recovery: &T,
    ) -> Result<(), JournalError> {
        self.save_record(chat_id, "recovery", recovery)
    }

    pub(crate) fn read_recovery<T: serde::de::DeserializeOwned>(
        &self,
        chat_id: &str,
    ) -> Result<Option<T>, JournalError> {
        Ok(self
            .read_record::<Option<T>>(chat_id, "recovery")?
            .flatten())
    }

    pub(crate) fn recovery_retired(&self, chat_id: &str) -> Result<bool, JournalError> {
        Ok(matches!(
            self.read_record::<Option<serde_json::Value>>(chat_id, "recovery")?,
            Some(None)
        ))
    }

    pub(crate) fn retire_recovery(&self, chat_id: &str) -> Result<(), JournalError> {
        self.save_record(chat_id, "recovery", &Option::<serde_json::Value>::None)
    }

    fn clear_recovery(&self, chat_id: &str) -> Result<(), JournalError> {
        let _guard = self.lock();
        match std::fs::remove_file(self.dir.join(format!("{}.recovery", sanitize_id(chat_id)))) {
            Ok(()) => File::open(&self.dir)?.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    pub(crate) fn recovery_sessions(&self) -> Result<Vec<String>, JournalError> {
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("recovery") {
                match self.chat_id_for(&path) {
                    Ok(Some(chat_id)) => match self.recovery_retired(&chat_id) {
                        Ok(false) => ids.push(chat_id),
                        Ok(true) => {}
                        Err(error) => {
                            tracing::error!(%chat_id, %error, "skipping invalid Crew recovery record")
                        }
                    },
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!(path = %path.display(), %error, "skipping invalid Crew recovery record")
                    }
                }
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Auto-resume revival budget (comet `resumeAttempt`/`MAX_AUTO_RESUME`):
    /// persisted beside the journal so a run that CRASHES THE ENGINE cannot
    /// revive itself in an infinite boot loop.
    pub fn resume_attempts(&self, chat_id: &str) -> u32 {
        std::fs::read_to_string(self.attempts_path(chat_id))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn note_resume_attempt(&self, chat_id: &str) -> u32 {
        let next = self.resume_attempts(chat_id) + 1;
        if let Err(err) = std::fs::write(self.attempts_path(chat_id), next.to_string()) {
            tracing::warn!(chat = %chat_id, error = %err, "resume-attempt ledger write failed");
        }
        next
    }

    /// A cleanly completed turn resets the budget — only consecutive
    /// crash-revive-crash cycles exhaust it.
    pub fn clear_resume_attempts(&self, chat_id: &str) {
        let _ = std::fs::remove_file(self.attempts_path(chat_id));
    }

    /// Append one event; returns its journal seq.
    pub fn append(&self, chat_id: &str, event: &AgentEvent) -> Result<u64, JournalError> {
        let mut files = self.lock();
        if !files.contains_key(chat_id) {
            // Bound the open-fd set: entries were never removed, so every chat
            // ever run held a descriptor for the process lifetime. Dropping is
            // safe — the next append reopens and rescans the tail. The cap
            // comfortably exceeds concurrent runs, so eviction stays rare.
            const OPEN_FILE_CAP: usize = 16;
            if files.len() >= OPEN_FILE_CAP {
                files.clear();
            }
            let path = self.path_for(chat_id);
            let (next_seq, needs_newline) = scan_tail(&path)?;
            let harness_session = read_harness_session(&path)?;
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            files.insert(
                chat_id.to_string(),
                ChatJournal {
                    file: BufWriter::with_capacity(8 * 1024, file),
                    next_seq,
                    needs_newline,
                    harness_session,
                },
            );
        }
        // Entry guaranteed present; avoid unwrap in a library path regardless.
        let Some(journal) = files.get_mut(chat_id) else {
            return Err(JournalError::Io(std::io::Error::other(
                "journal entry vanished under lock",
            )));
        };
        let seq = journal.next_seq;
        // Serialize the borrowed event through one bounded buffer. A progressive
        // tool call must not clone the payload, materialize a full JSON string,
        // then copy that string into a second whole-line allocation.
        if journal.needs_newline {
            journal.file.write_all(b"\n")?;
        }
        serde_json::to_writer(&mut journal.file, &JournalLine { seq, event })?;
        journal.file.write_all(b"\n")?;
        journal.file.flush()?;
        if matches!(event, AgentEvent::Done { .. }) {
            journal.file.get_ref().sync_all()?;
        }
        journal.needs_newline = false;
        journal.next_seq = seq + 1;
        journal.harness_session.observe(event);
        Ok(seq)
    }

    /// Events with `seq > after_seq`, in order. A cursor ahead of the last issued seq is
    /// from a previous era (file replaced) — falls back to a full replay, mirroring comet.
    pub fn replay(
        &self,
        chat_id: &str,
        after_seq: u64,
    ) -> Result<Vec<(u64, AgentEvent)>, JournalError> {
        let path = self.path_for(chat_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let all = read_lines(&path)?;
        let last_seq = all.last().map(|(seq, _)| *seq).unwrap_or(0);
        let from = if after_seq > last_seq { 0 } else { after_seq };
        Ok(all.into_iter().filter(|(seq, _)| *seq > from).collect())
    }

    /// The last event in a chat's journal, if any (ignores a torn tail line).
    pub fn last_event(&self, chat_id: &str) -> Result<Option<(u64, AgentEvent)>, JournalError> {
        let (last, _) = read_tail(&self.path_for(chat_id))?;
        Ok(last.map(|line| (line.seq,line.event)))
    }

    /// Latest provider binding, without replaying tool payloads. The capped
    /// writer cache includes absence and is updated only after durable append.
    pub fn last_harness_session(&self, chat_id: &str) -> Result<Option<(String, String)>, JournalError> {
        if let Some(journal) = self.lock().get(chat_id) {
            return Ok(journal.harness_session.last.clone());
        }
        Ok(read_harness_session(&self.path_for(chat_id))?.last)
    }

    fn chat_id_for(&self, path: &Path) -> Result<Option<String>, JournalError> {
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            return Ok(None);
        };
        // Filenames are sanitized; execution keys can contain ::session::. The
        // bound sidecar retains the actual key needed to open the same history.
        for extension in ["recovery", "context"] {
            match std::fs::read(path.with_extension(extension)) {
                Ok(bytes) => {
                    let (chat_id, _): (String, serde_json::Value) = serde_json::from_slice(&bytes)?;
                    if sanitize_id(&chat_id) != stem {
                        return Err(
                            std::io::Error::other("session_context_binding_mismatch").into()
                        );
                    }
                    return Ok(Some(chat_id));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Some(stem.to_string()))
    }

    /// Crash-recovery scan: chat ids whose journal's last event is NOT a `Done` — their
    /// runs died mid-stream and need recovery (stamp `aborted`, close the journal).
    pub fn stale_sessions(&self) -> Result<Vec<String>, JournalError> {
        let mut stale = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let chat_id = match self.chat_id_for(&path) {
                Ok(Some(chat_id)) => chat_id,
                Ok(None) => continue,
                Err(error) => {
                    tracing::error!(path = %path.display(), %error, "skipping invalid Crew journal binding");
                    continue;
                }
            };
            let last = match read_tail(&path) {
                Ok((event,_)) => event.map(|line| (line.seq,line.event)),
                Err(error) => {
                    tracing::error!(path = %path.display(), %error, "skipping unreadable Crew journal");
                    continue;
                }
            };
            match last {
                Some((_, AgentEvent::Done { .. })) | None => {}
                Some(_) => stale.push(chat_id),
            }
        }
        stale.sort();
        Ok(stale)
    }

    /// Chat ids with a journal in this identity-scoped store. A shared room can
    /// replicate chat documents, but it cannot create these local files.
    pub fn session_ids(&self) -> Result<Vec<String>, JournalError> {
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            match self.chat_id_for(&path) {
                Ok(Some(chat_id)) => ids.push(chat_id),
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(path = %path.display(), %error, "skipping invalid Crew journal binding")
                }
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// Remove a chat's journal file entirely (tests / future compaction).
    pub fn discard(&self, chat_id: &str) -> Result<(), JournalError> {
        self.lock().remove(chat_id);
        self.clear_recovery(chat_id)?;
        let context_path = self.dir.join(format!("{}.context", sanitize_id(chat_id)));
        match std::fs::remove_file(context_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let path = self.path_for(chat_id);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

/// Parse every valid line; malformed lines (torn tail writes) are skipped.
fn read_lines(path: &Path) -> Result<Vec<(u64, AgentEvent)>, JournalError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<JournalLine>(&line) {
            Ok(parsed) => out.push((parsed.seq, parsed.event)),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "journal: skipping malformed line");
            }
        }
    }
    Ok(out)
}

/// Cold/reopened metadata lookup reuses one line buffer and ignores unrelated
/// JSON values. Only real SessionStarted/Done records undergo event decoding;
/// malformed metadata and torn lines follow replay's existing skip contract.
fn read_harness_session(path: &Path) -> Result<HarnessSessionMetadata, JournalError> {
    #[derive(Deserialize)]
    struct EventKind {
        #[serde(rename = "type")]
        kind: String,
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HarnessSessionMetadata::default()),
        Err(error) => return Err(error.into()),
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut metadata = HarnessSessionMetadata::default();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 { break; }
        if line.trim().is_empty() { continue; }
        let parsed = serde_json::from_str::<JournalLine<EventKind>>(&line);
        match parsed {
            Ok(parsed) if matches!(parsed.event.kind.as_str(), "sessionStarted" | "done") => {
                match serde_json::from_str::<JournalLine>(&line) {
                    Ok(parsed) => metadata.observe(&parsed.event),
                    Err(error) => tracing::warn!(path = %path.display(), %error, "journal: skipping malformed metadata"),
                }
            }
            Ok(_) => {},
            Err(error) => tracing::warn!(path = %path.display(), %error, "journal: skipping malformed line"),
        }
    }
    Ok(metadata)
}

/// Read backwards in fixed blocks. Tail queries must not materialize all prior
/// tool argument prefixes merely to locate one event or its sequence number.
fn read_tail(path: &Path) -> Result<(Option<JournalLine>, bool), JournalError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None,false)),
        Err(error) => return Err(error.into()),
    };
    let mut position = file.metadata()?.len();
    if position == 0 { return Ok((None,false)); }
    file.seek(SeekFrom::Start(position-1))?;
    let mut final_byte=[0u8;1]; file.read_exact(&mut final_byte)?;
    let needs_newline=final_byte[0] != b'\n';
    let mut block=[0u8;8192];
    let mut line=Vec::new();
    while position > 0 {
        let count=position.min(block.len() as u64) as usize;
        position -= count as u64;
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut block[..count])?;
        for byte in block[..count].iter().rev() {
            if *byte == b'\n' {
                if !line.is_empty() {
                    line.reverse();
                    if let Ok(event) = serde_json::from_slice::<JournalLine>(&line) { return Ok((Some(event),needs_newline)); }
                    line.clear();
                }
            } else { line.push(*byte); }
        }
    }
    line.reverse();
    Ok((serde_json::from_slice::<JournalLine>(&line).ok(),needs_newline))
}

/// Next seq and torn-write isolation use the same bounded tail reader.
fn scan_tail(path: &Path) -> Result<(u64, bool), JournalError> {
    let (last,needs_newline)=read_tail(path)?;
    Ok((last.map_or(1,|line| line.seq+1),needs_newline))
}

/// Chat ids become file names; anything outside a conservative set is replaced so a
/// hostile id cannot traverse paths. (Ids are uuids in practice.)
fn sanitize_id(chat_id: &str) -> String {
    chat_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::DoneStatus;

    fn text(s: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: s.into() }
    }

    #[test]
    fn resume_metadata_preserves_bindings_without_replaying_tool_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = RunJournal::open(dir.path()).unwrap();
        let started = |id: &str, cwd: &str| AgentEvent::SessionStarted {
            harness: comet_proto::HarnessId::Mock, model: "test".into(), tools: Vec::new(),
            cwd: cwd.into(), session_id: id.into(), assistant_message_id: "assistant".into(),
        };
        let finished = |id: &str| AgentEvent::Done {
            status: DoneStatus::Completed, result: None, error: None, session_id: Some(id.into()),
        };
        let tool = AgentEvent::ToolCall { id: "large".into(), call: comet_proto::ToolCall::Exec {
            command: "{\"type\":\"sessionStarted\",\"sessionId\":\"spoof\",\"cwd\":\"/wrong\"}\nα".repeat(6000),
        } };
        assert_eq!(journal.last_harness_session("chat").unwrap(), None);
        journal.append("chat", &tool).unwrap();
        assert_eq!(journal.last_harness_session("chat").unwrap(), None);
        journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.last_harness_session("chat").unwrap(), None, "cold no-ID history must ignore embedded metadata");
        let mut expected = vec![(1, tool.clone())];
        for (event, binding) in [
            (finished("done-only"), ("done-only", "")),
            (started("early", "/early"), ("early", "/early")),
            (tool, ("early", "/early")),
            (finished("promoted"), ("promoted", "/early")),
            (started("", "/empty"), ("promoted", "/early")),
            (finished(""), ("promoted", "/early")),
            (finished("switched"), ("switched", "/empty")),
            (started("late", "/late"), ("late", "/late")),
            (done(), ("late", "/late")),
        ] {
            let seq = journal.append("chat", &event).unwrap();
            expected.push((seq, event));
            assert_eq!(journal.last_harness_session("chat").unwrap(), Some((binding.0.into(), binding.1.into())));
        }
        assert_eq!(journal.replay("chat", 0).unwrap(), expected);
        drop(journal);
        let mut file = OpenOptions::new().append(true).open(dir.path().join("chat.jsonl")).unwrap();
        file.write_all(b"{\"seq\":998,\"event\":{\"type\":\"sessionStarted\",\"cwd\":\"/bad\",\"sessionId\":\"invalid\"}}\n").unwrap();
        file.write_all(b"{\"seq\":999,\"event\":{\"type\":\"done\",\"status\":\"invalid\",\"sessionId\":\"invalid\"}}\n").unwrap();
        file.write_all(b"{\"seq\":1000,\"event\":{\"type\":\"sessionStarted\",\"sessionId\":\"torn\"").unwrap();
        drop(file);
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.last_harness_session("chat").unwrap(), Some(("late".into(), "/late".into())));
        assert_eq!(journal.replay("chat", 0).unwrap(), expected, "invalid metadata must not alter replay or routing");
        let event = finished("after-reopen");
        let seq = journal.append("chat", &event).unwrap();
        expected.push((seq, event));
        assert_eq!(journal.last_harness_session("chat").unwrap(), Some(("after-reopen".into(), "/late".into())));
        for index in 0..20 { journal.append(&format!("other-{index}"), &done()).unwrap(); }
        assert_eq!(journal.last_harness_session("chat").unwrap(), Some(("after-reopen".into(), "/late".into())));
        let event = started("after-eviction", "/evicted");
        let seq = journal.append("chat", &event).unwrap();
        expected.push((seq, event));
        assert_eq!(journal.last_harness_session("chat").unwrap(), Some(("after-eviction".into(), "/evicted".into())));
        assert_eq!(journal.replay("chat", 0).unwrap(), expected);
        journal.discard("chat").unwrap();
        assert_eq!(journal.last_harness_session("chat").unwrap(), None);
    }

    #[test]
    fn tail_lookup_handles_large_records_and_multiblock_torn_writes() {
        let dir=tempfile::tempdir().unwrap();
        let journal=RunJournal::open(dir.path()).unwrap();
        for prefix in 1..32 { journal.append("large",&text(&"history".repeat(prefix*1024))).unwrap(); }
        let command="αβγ tool arguments ".repeat(20_000);
        let last=AgentEvent::ToolCall { id:"last-tool".into(),call:comet_proto::ToolCall::Exec { command:command.clone() } };
        let seq=journal.append("large",&last).unwrap();
        assert_eq!(journal.last_event("large").unwrap(),Some((seq,last.clone())));
        drop(journal);
        let path=dir.path().join("large.jsonl");
        let mut file=OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"seq\":999,\"event\":\"").unwrap();
        file.write_all(&vec![b'x';24*1024]).unwrap(); drop(file);
        let reopened=RunJournal::open(dir.path()).unwrap();
        assert_eq!(reopened.last_event("large").unwrap(),Some((seq,last)));
        assert_eq!(reopened.append("large",&done()).unwrap(),seq+1);
        assert!(matches!(reopened.last_event("large").unwrap(),Some((n,AgentEvent::Done {..})) if n==seq+1));
        assert!(reopened.stale_sessions().unwrap().is_empty());
    }

    #[test]
    fn context_recovery_rejects_aliases_corruption_and_discarded_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal
            .save_context("chat::session::chat", &"accepted")
            .unwrap();
        assert!(
            journal
                .read_context::<String>("chat__session__chat")
                .is_err()
        );
        std::fs::write(dir.path().join("chat__session__chat.context"), b"truncated").unwrap();
        assert!(
            journal
                .read_context::<String>("chat::session::chat")
                .is_err()
        );
        journal
            .save_context("chat::session::chat", &"replacement")
            .unwrap();
        journal.discard("chat::session::chat").unwrap();
        assert!(
            journal
                .read_context::<String>("chat::session::chat")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pending_recovery_survives_done_and_reopen_until_retired() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        let request = serde_json::json!({"messageId": "original", "resume": "native-id", "attachments": ["/tmp/image.png"]});
        journal
            .save_recovery("chat::session::chat", &request)
            .unwrap();
        journal.append("chat::session::chat", &done()).unwrap();
        drop(journal);
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.session_ids().unwrap(), vec!["chat::session::chat"]);
        assert_eq!(
            journal.recovery_sessions().unwrap(),
            vec!["chat::session::chat"]
        );
        assert_eq!(
            journal
                .read_recovery::<serde_json::Value>("chat::session::chat")
                .unwrap(),
            Some(request)
        );
        assert!(
            journal
                .read_recovery::<serde_json::Value>("chat__session__chat")
                .is_err()
        );
        journal.retire_recovery("chat::session::chat").unwrap();
        drop(journal);
        assert!(
            RunJournal::open(dir.path())
                .unwrap()
                .recovery_sessions()
                .unwrap()
                .is_empty()
        );
    }

    fn done() -> AgentEvent {
        AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
        }
    }

    #[test]
    fn large_escaped_events_flush_from_bounded_journal_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        let command = "α\"quoted\"\\path\n\t".repeat(24 * 1024);
        assert!(command.len() > 300 * 1024);
        let event = AgentEvent::ToolCall {
            id: "large-tool".into(),
            call: comet_proto::ToolCall::Exec { command },
        };
        assert_eq!(journal.append("large", &event).unwrap(), 1);
        assert!(journal.lock().get("large").unwrap().file.capacity() <= 8 * 1024);
        // Separate readers must see the complete event before the writer is
        // dropped, including escapes and UTF-8 spanning buffer boundaries.
        assert_eq!(journal.last_event("large").unwrap(), Some((1, event.clone())));
        assert_eq!(journal.replay("large", 0).unwrap(), vec![(1, event)]);
        assert_eq!(journal.append("large", &done()).unwrap(), 2);
        drop(journal);
        let reopened = RunJournal::open(dir.path()).unwrap();
        assert_eq!(reopened.append("large", &text("next")).unwrap(), 3);
        assert_eq!(reopened.last_event("large").unwrap(), Some((3, text("next"))));
    }

    #[test]
    fn appends_are_monotonic_and_replayable() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.append("chat-1", &text("a")).unwrap(), 1);
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
        assert_eq!(journal.append("chat-1", &done()).unwrap(), 3);

        let all = journal.replay("chat-1", 0).unwrap();
        assert_eq!(all.len(), 3);
        let after = journal.replay("chat-1", 2).unwrap();
        assert_eq!(after.len(), 1);
        assert!(matches!(after[0].1, AgentEvent::Done { .. }));
        // Era fallback: cursor ahead of last seq replays everything.
        assert_eq!(journal.replay("chat-1", 99).unwrap().len(), 3);
    }

    #[test]
    fn seq_continues_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = RunJournal::open(dir.path()).unwrap();
            journal.append("chat-1", &text("a")).unwrap();
        }
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
    }

    #[test]
    fn stale_scan_flags_journals_without_terminal_done() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("dead", &text("partial")).unwrap();
        journal.append("clean", &text("full")).unwrap();
        journal.append("clean", &done()).unwrap();
        assert_eq!(journal.stale_sessions().unwrap(), vec!["dead".to_string()]);
        // Closing the stale journal with a Done clears the flag.
        journal.append("dead", &done()).unwrap();
        assert!(journal.stale_sessions().unwrap().is_empty());
    }

    #[test]
    fn session_ids_list_identity_local_journals() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("chat-b", &text("b")).unwrap();
        journal.append("chat-a", &text("a")).unwrap();
        journal.append("chat-a", &done()).unwrap();
        std::fs::write(dir.path().join("ignored.resume"), "1").unwrap();

        assert_eq!(journal.session_ids().unwrap(), vec!["chat-a", "chat-b"]);
    }

    #[test]
    fn torn_tail_line_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = RunJournal::open(dir.path()).unwrap();
            journal.append("chat-1", &text("a")).unwrap();
        }
        // Simulate a crash mid-write: garbage with no trailing newline.
        let path = dir.path().join("chat-1.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"seq\":2,\"event\":{\"type\":\"textD")
            .unwrap();
        drop(f);

        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.replay("chat-1", 0).unwrap().len(), 1);
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
        let all = journal.replay("chat-1", 0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].0, 2);
    }
}
