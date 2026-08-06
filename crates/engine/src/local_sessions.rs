//! Discovery and one-time import of harness-native local session transcripts.
//!
//! Local CLI stores remain the source of truth until the user explicitly imports a
//! candidate. Import copies user/assistant text into a history-only Comet chat;
//! file discovery alone never claims control of a native session writer.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{DateTime, Utc};
use comet_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use comet_proto::{
    ChatConfig, HarnessId, LocalSessionAttachResult, LocalSessionCandidate, ReasoningLevel,
    SandboxLevel,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{DocHost, EngineError, WorkspaceHost};

const MAX_CANDIDATES_PER_HARNESS: usize = 100;
const PREFIX_BYTES: u64 = 512 * 1024;
const MAX_JSONL_RECORD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
struct DiscoveredSession {
    candidate: LocalSessionCandidate,
    path: PathBuf,
}

/// Find recent Claude Code, Codex, and OMP sessions owned by this device.
/// Missing or malformed stores are ignored independently.
pub fn discover() -> Vec<LocalSessionCandidate> {
    let mut sessions = discover_with_roots(&session_roots());
    sessions.sort_by(|a, b| {
        b.candidate
            .updated_at
            .cmp(&a.candidate.updated_at)
            .then_with(|| a.candidate.id.cmp(&b.candidate.id))
    });
    sessions
        .into_iter()
        .map(|session| session.candidate)
        .collect()
}

/// Resolve an opaque candidate id again, import its transcript idempotently, and
/// return the Comet chat/space selected by the UI.
pub fn attach(
    candidate_id: &str,
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
) -> Result<LocalSessionAttachResult, EngineError> {
    let session = discover_with_roots(&session_roots())
        .into_iter()
        .find(|session| session.candidate.id == candidate_id)
        .ok_or_else(|| EngineError::Other("local session is no longer available".into()))?;
    attach_discovered(session, workspace, doc_host)
}

fn attach_discovered(
    session: DiscoveredSession,
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
) -> Result<LocalSessionAttachResult, EngineError> {
    let candidate = &session.candidate;
    let device_id = doc_host.device_id();
    let space_id = workspace
        .read_spaces()?
        .into_iter()
        .find(|space| space.device_id == device_id && space.path == candidate.cwd)
        .map(|space| space.id)
        .unwrap_or_else(|| format!("local-space-{}", short_hash(&candidate.cwd)));

    if !workspace
        .read_spaces()?
        .iter()
        .any(|space| space.id == space_id)
    {
        let git_detected = Path::new(&candidate.cwd).join(".git").exists();
        workspace.create_space(&space_id, device_id, &candidate.cwd, None, git_detected)?;
    }

    let chat_id = candidate.chat_id.clone();
    let entries = load_transcript(&session, device_id)?;
    let doc = doc_host.open(&chat_id)?;
    let session_doc = doc.doc_arc();
    let existing: HashSet<String> = session_doc
        .read_entries()?
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    for entry in &entries {
        if !existing.contains(&entry.id) {
            session_doc.push_message(entry)?;
        }
    }

    let config = ChatConfig {
        harness: candidate.harness,
        model: candidate.model.clone(),
        reasoning: candidate.reasoning,
        model_options: serde_json::Map::new(),
        sandbox: SandboxLevel::WorkspaceWrite,
    };
    workspace.create_chat(
        &chat_id,
        &space_id,
        Some(config.clone()),
        Some(candidate.cwd.clone()),
    )?;
    workspace.rename_chat(&chat_id, &candidate.title)?;
    workspace.set_chat_config(&chat_id, &config)?;
    // Discovery proves history only. It does not prove exclusive writer ownership,
    // so never seed automatic native resume from files alone.
    if let Some(preview) = entries
        .last()
        .and_then(|entry| entry.parts.iter().find_map(text_part))
        .or(candidate.preview.as_deref())
    {
        workspace.note_message(&chat_id, preview);
    }
    workspace.set_chat_activity(
        &chat_id,
        Some(candidate.updated_at),
        Some(candidate.created_at),
    )?;

    Ok(LocalSessionAttachResult { chat_id, space_id })
}

#[derive(Debug, Clone)]
struct SessionRoots {
    claude: PathBuf,
    codex: PathBuf,
    omp: PathBuf,
}

fn session_roots() -> SessionRoots {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    SessionRoots {
        claude: std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude")),
        codex: std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex")),
        omp: std::env::var_os("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".omp/agent")),
    }
}

fn discover_with_roots(roots: &SessionRoots) -> Vec<DiscoveredSession> {
    let mut sessions = Vec::new();
    sessions.extend(discover_claude(&roots.claude));
    sessions.extend(discover_codex(&roots.codex));
    sessions.extend(discover_omp(&roots.omp));
    sessions.retain(|session| {
        !session
            .candidate
            .title
            .starts_with("Reply with ONLY a concise 3-5 word title")
    });
    sessions
}

fn discover_claude(root: &Path) -> Vec<DiscoveredSession> {
    recent_jsonl_files(&root.join("projects"), MAX_CANDIDATES_PER_HARNESS)
        .into_iter()
        .filter_map(|path| candidate_from_claude(&path))
        .collect()
}

fn candidate_from_claude(path: &Path) -> Option<DiscoveredSession> {
    let values = read_prefix_values(path);
    let mut session_id = path.file_stem()?.to_str()?.to_string();
    let mut cwd = None;
    let mut title = None;
    let mut preview = None;
    let mut model = None;
    let mut created_at = None;
    let mut updated_at = file_modified_ms(path);

    for value in values {
        session_id = string_at(&value, &["sessionId"])
            .unwrap_or(session_id.as_str())
            .to_string();
        cwd = cwd.or_else(|| string_at(&value, &["cwd"]).map(str::to_string));
        title = title.or_else(|| {
            string_at(&value, &["customTitle"])
                .or_else(|| string_at(&value, &["slug"]))
                .map(str::to_string)
        });
        model = model.or_else(|| string_at(&value, &["message", "model"]).map(str::to_string));
        if string_at(&value, &["type"]) == Some("user")
            && !value
                .get("isMeta")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            preview =
                preview.or_else(|| value.get("message").and_then(message_text).map(short_text));
        }
        if let Some(ms) = parse_timestamp(string_at(&value, &["timestamp"])) {
            created_at = Some(created_at.map_or(ms, |old: i64| old.min(ms)));
            updated_at = Some(updated_at.map_or(ms, |old| old.max(ms)));
        }
    }

    build_session(
        HarnessId::ClaudeCode,
        session_id,
        cwd?,
        title,
        preview,
        model,
        None,
        created_at,
        updated_at,
        path,
    )
}

fn discover_codex(root: &Path) -> Vec<DiscoveredSession> {
    let db_path = root.join("state_5.sqlite");
    let from_db = discover_codex_db(&db_path);
    if !from_db.is_empty() {
        return from_db;
    }
    let mut files = recent_jsonl_files(&root.join("sessions"), MAX_CANDIDATES_PER_HARNESS);
    files.extend(recent_jsonl_files(
        &root.join("archived_sessions"),
        MAX_CANDIDATES_PER_HARNESS,
    ));
    files.sort_by_key(|path| std::cmp::Reverse(file_modified_ms(path).unwrap_or_default()));
    files.truncate(MAX_CANDIDATES_PER_HARNESS);
    files
        .into_iter()
        .filter_map(|path| candidate_from_codex_rollout(&path, None))
        .collect()
}

fn discover_codex_db(path: &Path) -> Vec<DiscoveredSession> {
    let Ok(connection) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "SELECT id, rollout_path, created_at, updated_at, cwd, title, model, reasoning_effort \
         FROM threads WHERE archived = 0 ORDER BY updated_at DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([MAX_CANDIDATES_PER_HARNESS as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    }) else {
        return Vec::new();
    };

    rows.filter_map(Result::ok)
        .filter_map(
            |(session_id, rollout_path, created_at, updated_at, cwd, title, model, reasoning)| {
                let path = PathBuf::from(rollout_path);
                if !path.is_file() || cwd.trim().is_empty() {
                    return None;
                }
                let mut discovered = candidate_from_codex_rollout(&path, Some(&session_id))?;
                discovered.candidate.cwd = cwd;
                if !title.trim().is_empty() {
                    discovered.candidate.title = short_text(title);
                }
                discovered.candidate.model = model.or(discovered.candidate.model);
                discovered.candidate.reasoning = reasoning.as_deref().and_then(parse_reasoning);
                discovered.candidate.created_at = normalize_epoch(created_at);
                discovered.candidate.updated_at = normalize_epoch(updated_at);
                Some(discovered)
            },
        )
        .collect()
}

fn candidate_from_codex_rollout(path: &Path, known_id: Option<&str>) -> Option<DiscoveredSession> {
    let values = read_prefix_values(path);
    let mut session_id = known_id.map(str::to_string);
    let mut cwd = None;
    let mut preview = None;
    let mut model = None;
    let mut created_at = None;
    let mut updated_at = file_modified_ms(path);

    for value in values {
        if string_at(&value, &["type"]) == Some("session_meta") {
            session_id = session_id.or_else(|| {
                string_at(&value, &["payload", "id"])
                    .or_else(|| string_at(&value, &["id"]))
                    .map(str::to_string)
            });
            cwd = cwd.or_else(|| string_at(&value, &["payload", "cwd"]).map(str::to_string));
        }
        if string_at(&value, &["type"]) == Some("response_item") {
            let payload = value.get("payload");
            if string_at_opt(payload, &["type"]) == Some("message")
                && string_at_opt(payload, &["role"]) == Some("user")
            {
                preview = preview.or_else(|| payload.and_then(message_text).map(short_text));
            }
        }
        model = model.or_else(|| {
            string_at(&value, &["payload", "model"])
                .or_else(|| string_at(&value, &["model"]))
                .map(str::to_string)
        });
        if let Some(ms) = parse_timestamp(string_at(&value, &["timestamp"])) {
            created_at = Some(created_at.map_or(ms, |old: i64| old.min(ms)));
            updated_at = Some(updated_at.map_or(ms, |old| old.max(ms)));
        }
    }

    let session_id = session_id.or_else(|| path.file_stem()?.to_str().map(str::to_string))?;
    build_session(
        HarnessId::Codex,
        session_id,
        cwd?,
        preview.clone(),
        preview,
        model,
        None,
        created_at,
        updated_at,
        path,
    )
}

fn discover_omp(root: &Path) -> Vec<DiscoveredSession> {
    recent_jsonl_files(&root.join("sessions"), MAX_CANDIDATES_PER_HARNESS)
        .into_iter()
        .filter_map(|path| candidate_from_omp(&path))
        .collect()
}

fn candidate_from_omp(path: &Path) -> Option<DiscoveredSession> {
    let values = read_prefix_values(path);
    let mut session_id = None;
    let mut cwd = None;
    let mut title = None;
    let mut preview = None;
    let mut model = None;
    let mut created_at = None;
    let mut updated_at = file_modified_ms(path);

    for value in values {
        match string_at(&value, &["type"]) {
            Some("session") => {
                session_id = string_at(&value, &["id"])
                    .map(str::to_string)
                    .or(session_id);
                cwd = string_at(&value, &["cwd"]).map(str::to_string).or(cwd);
            }
            Some("custom-title") => {
                title = string_at(&value, &["customTitle"])
                    .map(str::to_string)
                    .or(title);
            }
            Some("message") => {
                let message = value.get("message");
                if string_at_opt(message, &["role"]) == Some("user") {
                    preview = preview.or_else(|| message.and_then(message_text).map(short_text));
                }
                model = model.or_else(|| {
                    string_at_opt(message, &["model"])
                        .or_else(|| string_at(&value, &["model"]))
                        .map(str::to_string)
                });
            }
            _ => {}
        }
        if let Some(ms) = parse_timestamp(string_at(&value, &["timestamp"])) {
            created_at = Some(created_at.map_or(ms, |old: i64| old.min(ms)));
            updated_at = Some(updated_at.map_or(ms, |old| old.max(ms)));
        }
    }

    let session_id = session_id.or_else(|| path.file_stem()?.to_str().map(str::to_string))?;
    build_session(
        HarnessId::Omp,
        session_id,
        cwd?,
        title,
        preview,
        model,
        None,
        created_at,
        updated_at,
        path,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_session(
    harness: HarnessId,
    session_id: String,
    cwd: String,
    title: Option<String>,
    preview: Option<String>,
    model: Option<String>,
    reasoning: Option<ReasoningLevel>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    path: &Path,
) -> Option<DiscoveredSession> {
    if session_id.trim().is_empty() || cwd.trim().is_empty() || !path.is_file() {
        return None;
    }
    let preview = preview.filter(|text| !text.trim().is_empty());
    let title = title
        .filter(|text| !text.trim().is_empty())
        .map(short_text)
        .or_else(|| preview.clone())
        .unwrap_or_else(|| format!("{} session", harness_label(harness)));
    let updated_at = updated_at.unwrap_or_else(|| Utc::now().timestamp_millis());
    let created_at = created_at.unwrap_or(updated_at);
    let id = format!("{}:{}", harness_key(harness), short_hash(&session_id));
    let chat_id = format!("local-chat-{}", short_hash(&id));
    Some(DiscoveredSession {
        candidate: LocalSessionCandidate {
            id,
            chat_id,
            harness,
            session_id,
            cwd,
            title,
            preview,
            model,
            reasoning,
            created_at,
            updated_at,
            // Files/PIDs alone never establish live control or exclusive writer ownership.
            live_attachable: false,
            resumable: false,
            history_only: true,
            busy_elsewhere: None,
        },
        path: path.to_path_buf(),
    })
}

fn load_transcript(
    session: &DiscoveredSession,
    device_id: &str,
) -> Result<Vec<SessionMessageEntry>, EngineError> {
    let file = File::open(&session.path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut entries = Vec::new();
    let mut ordinal = 0_i64;

    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        ordinal += 1;
        if line.len() > MAX_JSONL_RECORD_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let extracted = match session.candidate.harness {
            HarnessId::ClaudeCode => transcript_record_claude(&value),
            HarnessId::Codex => transcript_record_codex(&value),
            HarnessId::Omp => transcript_record_omp(&value),
            _ => None,
        };
        let Some((source_id, role, text, timestamp)) = extracted else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let source_id = source_id.unwrap_or_else(|| ordinal.to_string());
        let entry_id = format!(
            "local-import-{}",
            short_hash(&format!("{}:{source_id}", session.candidate.id))
        );
        entries.push(SessionMessageEntry {
            id: entry_id.clone(),
            role,
            parts: vec![MessagePart::Text {
                id: format!("{entry_id}-text"),
                text: text.to_string(),
            }],
            created_at: timestamp.unwrap_or(session.candidate.created_at + ordinal),
            device_id: device_id.to_string(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        });
    }
    entries.sort_by_key(|entry| entry.created_at);
    Ok(entries)
}

fn transcript_record_claude(
    value: &Value,
) -> Option<(Option<String>, MessageRole, String, Option<i64>)> {
    let kind = string_at(value, &["type"])?;
    if !matches!(kind, "user" | "assistant")
        || value
            .get("isMeta")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || value
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return None;
    }
    let role = if kind == "user" {
        MessageRole::User
    } else {
        MessageRole::Assistant
    };
    Some((
        string_at(value, &["uuid"]).map(str::to_string),
        role,
        message_text(value.get("message")?)?,
        parse_timestamp(string_at(value, &["timestamp"])),
    ))
}

fn transcript_record_codex(
    value: &Value,
) -> Option<(Option<String>, MessageRole, String, Option<i64>)> {
    if string_at(value, &["type"]) != Some("response_item") {
        return None;
    }
    let payload = value.get("payload")?;
    if string_at_opt(Some(payload), &["type"]) != Some("message") {
        return None;
    }
    let role = parse_role(string_at_opt(Some(payload), &["role"])?)?;
    Some((
        string_at_opt(Some(payload), &["id"])
            .or_else(|| string_at(value, &["id"]))
            .map(str::to_string),
        role,
        message_text(payload)?,
        parse_timestamp(string_at(value, &["timestamp"])),
    ))
}

fn transcript_record_omp(
    value: &Value,
) -> Option<(Option<String>, MessageRole, String, Option<i64>)> {
    if string_at(value, &["type"]) != Some("message") {
        return None;
    }
    let message = value.get("message")?;
    let role = parse_role(string_at_opt(Some(message), &["role"])?)?;
    Some((
        string_at(value, &["id"])
            .or_else(|| string_at_opt(Some(message), &["id"]))
            .map(str::to_string),
        role,
        message_text(message)?,
        parse_timestamp(string_at(value, &["timestamp"]))
            .or_else(|| parse_timestamp(string_at_opt(Some(message), &["timestamp"]))),
    ))
}

fn parse_role(role: &str) -> Option<MessageRole> {
    match role {
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        _ => None,
    }
}

fn message_text(value: &Value) -> Option<String> {
    let content = value.get("content").unwrap_or(value);
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let joined = parts
                .iter()
                .filter_map(|part| {
                    let kind = string_at(part, &["type"]);
                    if kind
                        .is_some_and(|kind| !matches!(kind, "text" | "input_text" | "output_text"))
                    {
                        return None;
                    }
                    string_at(part, &["text"])
                        .or_else(|| part.as_str())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!joined.trim().is_empty()).then_some(joined)
        }
        _ => None,
    }
}

fn text_part(part: &MessagePart) -> Option<&str> {
    match part {
        MessagePart::Text { text, .. } => Some(text.as_str()),
        _ => None,
    }
}

fn read_prefix_values(path: &Path) -> Vec<Value> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if file.take(PREFIX_BYTES).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn recent_jsonl_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    fn visit(path: &Path, files: &mut Vec<(i64, PathBuf)>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                visit(&path, files);
            } else if file_type.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            {
                files.push((file_modified_ms(&path).unwrap_or_default(), path));
            }
        }
    }

    let mut files = Vec::new();
    visit(root, &mut files);
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files.truncate(limit);
    files.into_iter().map(|(_, path)| path).collect()
}

fn file_modified_ms(path: &Path) -> Option<i64> {
    let duration = fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    i64::try_from(duration.as_millis()).ok()
}

fn string_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    string_at_opt(Some(value), path)
}

fn string_at_opt<'a>(mut value: Option<&'a Value>, path: &[&str]) -> Option<&'a str> {
    for key in path {
        value = value?.get(*key);
    }
    value?.as_str()
}

fn parse_timestamp(value: Option<&str>) -> Option<i64> {
    DateTime::parse_from_rfc3339(value?)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
}

fn normalize_epoch(value: i64) -> i64 {
    if value.abs() < 100_000_000_000 {
        value.saturating_mul(1_000)
    } else {
        value
    }
}

fn parse_reasoning(value: &str) -> Option<ReasoningLevel> {
    serde_json::from_value(Value::String(value.to_ascii_lowercase())).ok()
}

fn short_text(text: impl AsRef<str>) -> String {
    text.as_ref()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

fn short_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn harness_key(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "claude",
        HarnessId::Codex => "codex",
        HarnessId::Omp => "omp",
        HarnessId::Cursor => "cursor",
        HarnessId::Mock => "mock",
    }
}

fn harness_label(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Claude Code",
        HarnessId::Codex => "Codex",
        HarnessId::Omp => "OMP",
        HarnessId::Cursor => "Cursor",
        HarnessId::Mock => "Test",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;

    fn fixture(root: &Path, relative: &str, lines: &[Value]) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn discovers_all_three_native_session_formats() {
        let temp = TempDir::new().unwrap();
        let roots = SessionRoots {
            claude: temp.path().join("claude"),
            codex: temp.path().join("codex"),
            omp: temp.path().join("omp"),
        };
        fixture(
            &roots.claude,
            "projects/repo/claude-1.jsonl",
            &[
                serde_json::json!({"type":"custom-title","customTitle":"Claude plan","sessionId":"claude-1"}),
                serde_json::json!({"type":"user","sessionId":"claude-1","cwd":"/repo","timestamp":"2026-08-05T10:00:00Z","message":{"role":"user","content":"Plan the API"}}),
                serde_json::json!({"type":"assistant","sessionId":"claude-1","cwd":"/repo","timestamp":"2026-08-05T10:01:00Z","message":{"role":"assistant","model":"claude-fable-5","content":[{"type":"text","text":"Here is the plan"}]}}),
            ],
        );
        fixture(
            &roots.codex,
            "sessions/2026/08/05/rollout-codex-1.jsonl",
            &[
                serde_json::json!({"type":"session_meta","timestamp":"2026-08-05T11:00:00Z","payload":{"id":"codex-1","cwd":"/repo"}}),
                serde_json::json!({"type":"response_item","timestamp":"2026-08-05T11:01:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Review the change"}]}}),
            ],
        );
        fixture(
            &roots.codex,
            "sessions/2026/08/05/rollout-title-helper.jsonl",
            &[
                serde_json::json!({"type":"session_meta","timestamp":"2026-08-05T11:02:00Z","payload":{"id":"title-helper","cwd":"/repo"}}),
                serde_json::json!({"type":"response_item","timestamp":"2026-08-05T11:02:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Reply with ONLY a concise 3-5 word title in Title Case"}]}}),
            ],
        );
        fixture(
            &roots.omp,
            "sessions/repo/omp-1.jsonl",
            &[
                serde_json::json!({"type":"session","id":"omp-1","cwd":"/repo","timestamp":"2026-08-05T12:00:00Z"}),
                serde_json::json!({"type":"custom-title","customTitle":"OMP review"}),
                serde_json::json!({"type":"message","id":"m1","timestamp":"2026-08-05T12:01:00Z","message":{"role":"user","content":[{"type":"text","text":"Check the UI"}]}}),
            ],
        );

        let sessions = discover_with_roots(&roots);
        assert_eq!(sessions.len(), 3);
        assert!(sessions.iter().all(|session| {
            !session.candidate.live_attachable
                && !session.candidate.resumable
                && session.candidate.history_only
                && session.candidate.busy_elsewhere.is_none()
        }));
        assert!(sessions.iter().any(|session| {
            session.candidate.harness == HarnessId::ClaudeCode
                && session.candidate.title == "Claude plan"
                && session.candidate.model.as_deref() == Some("claude-fable-5")
        }));
        assert!(sessions.iter().any(|session| {
            session.candidate.harness == HarnessId::Codex
                && session.candidate.title == "Review the change"
        }));
        assert!(sessions.iter().any(|session| {
            session.candidate.harness == HarnessId::Omp && session.candidate.title == "OMP review"
        }));
    }

    #[test]
    fn imports_only_user_and_assistant_text_with_stable_ids() {
        let temp = TempDir::new().unwrap();
        let path = fixture(
            temp.path(),
            "session.jsonl",
            &[
                serde_json::json!({"type":"session","id":"omp-1","cwd":"/repo","timestamp":"2026-08-05T12:00:00Z"}),
                serde_json::json!({"type":"message","id":"u1","timestamp":"2026-08-05T12:01:00Z","message":{"role":"user","content":[{"type":"text","text":"Question"}]}}),
                serde_json::json!({"type":"message","id":"a1","timestamp":"2026-08-05T12:02:00Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret"},{"type":"text","text":"Answer"},{"type":"toolCall","name":"bash"}]}}),
                serde_json::json!({"type":"message","id":"t1","timestamp":"2026-08-05T12:03:00Z","message":{"role":"toolResult","content":[{"type":"text","text":"ignored"}]}}),
            ],
        );
        let session = candidate_from_omp(&path).unwrap();
        let first = load_transcript(&session, "device-1").unwrap();
        let second = load_transcript(&session, "device-1").unwrap();

        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].role, MessageRole::User);
        assert_eq!(first[1].role, MessageRole::Assistant);
        assert_eq!(text_part(&first[1].parts[0]), Some("Answer"));
    }
}
