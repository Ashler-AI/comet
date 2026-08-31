//! Discovery and on-demand import of harness-native local session transcripts.
//!
//! Local CLI stores remain the transcript source of truth. Listing reads bounded
//! metadata only; transcripts and workspace rows are materialized by an explicit
//! attach.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::UNIX_EPOCH;

use chrono::{DateTime, Utc};
use comet_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use comet_proto::{
    ChatConfig, HarnessId, LocalSessionAttachResult, LocalSessionCandidate, OmpSessionArtifact,
    ReasoningLevel, SandboxLevel,
};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{DocHost, EngineError, WorkspaceHost};

const MAX_CANDIDATES_PER_HARNESS: usize = 100;
const PREFIX_BYTES: u64 = 64 * 1024;
const MAX_DISCOVERY_DEPTH: usize = 8;
const MAX_JSONL_RECORD_BYTES: usize = 4 * 1024 * 1024;
/// OMP journals are append-only trees. Resolving the active branch retains
/// only record ids and parent ids, never full JSON values or message bodies.
const MAX_TRANSCRIPT_GRAPH_RECORDS: usize = 500_000;
const MAX_TRANSCRIPT_GRAPH_ID_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
struct DiscoveredSession {
    candidate: LocalSessionCandidate,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptImport {
    None,
    Full,
    After(i64),
}

/// Find recent Claude Code, Codex, OMP, Prime Agent, and OpenCode sessions.
/// Up-to-date imported chats and Comet-owned native sessions are omitted; a
/// stale imported OMP chat remains available for an explicit history refresh.
/// The scan never mutates workspace state, opens a session doc, or materializes
/// transcript entries.
pub fn list(workspace: &WorkspaceHost) -> Result<Vec<LocalSessionCandidate>, EngineError> {
    list_discovered(sorted_discovered(), workspace)
}

fn materialized_session_needs_action(
    candidate: &LocalSessionCandidate,
    last_imported_at: i64,
    native_owned: bool,
) -> bool {
    candidate.harness == HarnessId::Omp
        && !(native_owned && !candidate.resumable)
        && (candidate.updated_at > last_imported_at || (candidate.resumable && !native_owned))
}

fn list_discovered(
    mut sessions: Vec<DiscoveredSession>,
    workspace: &WorkspaceHost,
) -> Result<Vec<LocalSessionCandidate>, EngineError> {
    let chats = workspace.doc().read_chats()?;
    let materialized_chat_activity: HashMap<&str, i64> = chats
        .iter()
        .map(|chat| {
            (
                chat.id.as_str(),
                chat.last_message_at
                    .map(|at| at.timestamp_millis())
                    .unwrap_or(i64::MIN),
            )
        })
        .collect();
    let owned_native_ids: HashSet<&str> = chats
        .iter()
        .filter_map(|chat| chat.harness_session_id.as_deref())
        .filter(|id| !id.is_empty())
        .collect();
    sessions.retain(|session| {
        let native_owned = owned_native_ids.contains(session.candidate.session_id.as_str())
            || owned_native_ids.contains(session.path.to_string_lossy().as_ref());
        if let Some(last_imported_at) =
            materialized_chat_activity.get(session.candidate.chat_id.as_str())
        {
            return materialized_session_needs_action(
                &session.candidate,
                *last_imported_at,
                native_owned,
            );
        }
        !native_owned
    });
    Ok(sessions
        .into_iter()
        .map(|session| session.candidate)
        .collect())
}

fn sorted_discovered() -> Vec<DiscoveredSession> {
    let mut sessions = discover_with_roots(&session_roots());
    sessions.sort_by(|a, b| {
        b.candidate
            .updated_at
            .cmp(&a.candidate.updated_at)
            .then_with(|| a.candidate.id.cmp(&b.candidate.id))
    });
    sessions
}

/// Re-resolve an opaque OMP candidate and capture its exact native session file.
///
/// This local-controller API does not accept arbitrary filesystem paths. The
/// discovery id must still resolve to an OMP candidate, and capture revalidates
/// the native id/cwd against the bytes under a stable no-follow file handle.
pub fn capture_omp_artifact(candidate_id: &str) -> Result<OmpSessionArtifact, EngineError> {
    capture_omp_artifact_with_roots(candidate_id, &session_roots())
}

/// Capture the OMP session identified by the durable native id and cwd stored
/// on a Crew chat row.
///
/// Re-discovery resolves that trusted pair to a concrete OMP candidate, then
/// capture revalidates the same identity against the bytes under a stable
/// no-follow file handle. Callers never provide a filesystem path.
pub fn capture_omp_artifact_for_session(
    native_session_id: &str,
    cwd: &str,
) -> Result<OmpSessionArtifact, EngineError> {
    capture_omp_artifact_for_session_with_roots(native_session_id, cwd, &session_roots())
}

pub(crate) fn capture_omp_file_for_session(
    native_session_id: &str,
    cwd: &str,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<crate::omp_session_artifact::CapturedOmpSessionFile, EngineError> {
    let roots = session_roots();
    if cancellation.is_cancelled() {
        return Err(EngineError::Other(
            "OMP session capture was cancelled".into(),
        ));
    }
    let session = find_omp_session_for_capture(&roots.omp, native_session_id, cwd, cancellation)?;
    let sessions_root = roots.omp.join("sessions");
    let storage_relative_path = session.path.strip_prefix(&sessions_root).map_err(|_| {
        EngineError::Other("OMP session path escaped the configured sessions root".into())
    })?;
    crate::omp_session_artifact::capture_omp_session_file(
        &session.path,
        storage_relative_path,
        &session.candidate.session_id,
        &session.candidate.cwd,
        cancellation,
    )
}

fn find_omp_session_for_capture(
    root: &Path,
    native_session_id: &str,
    cwd: &str,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<DiscoveredSession, EngineError> {
    fn matching_session(
        directory: &Path,
        native_session_id: &str,
        cwd: &str,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<Option<DiscoveredSession>, EngineError> {
        let Ok(entries) = fs::read_dir(directory) else {
            return Ok(None);
        };
        for entry in entries.flatten() {
            if cancellation.is_cancelled() {
                return Err(EngineError::Other(
                    "OMP session capture was cancelled".into(),
                ));
            }
            let path = entry.path();
            if !entry.file_type().is_ok_and(|file_type| file_type.is_file())
                || path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
            {
                continue;
            }
            let Some(session) = candidate_from_omp_with_writer_state(
                &path,
                comet_harness::omp::SessionWriterState::Unknown,
            ) else {
                continue;
            };
            if session.candidate.session_id == native_session_id && session.candidate.cwd == cwd {
                return Ok(Some(session));
            }
        }
        Ok(None)
    }

    let sessions_root = root.join("sessions");
    if let Some(session) = matching_session(&sessions_root, native_session_id, cwd, cancellation)? {
        return Ok(session);
    }
    let entries = fs::read_dir(&sessions_root)
        .map_err(|_| EngineError::Other("local OMP session is no longer available".into()))?;
    for entry in entries.flatten() {
        if cancellation.is_cancelled() {
            return Err(EngineError::Other(
                "OMP session capture was cancelled".into(),
            ));
        }
        if entry.file_type().is_ok_and(|file_type| file_type.is_dir())
            && let Some(session) =
                matching_session(&entry.path(), native_session_id, cwd, cancellation)?
        {
            return Ok(session);
        }
    }
    Err(EngineError::Other(
        "local OMP session is no longer available".into(),
    ))
}

fn capture_omp_artifact_with_roots(
    candidate_id: &str,
    roots: &SessionRoots,
) -> Result<OmpSessionArtifact, EngineError> {
    let session = discover_with_roots(roots)
        .into_iter()
        .find(|session| session.candidate.id == candidate_id)
        .ok_or_else(|| EngineError::Other("local session is no longer available".into()))?;
    capture_discovered_omp_artifact(&session, roots)
}

fn capture_omp_artifact_for_session_with_roots(
    native_session_id: &str,
    cwd: &str,
    roots: &SessionRoots,
) -> Result<OmpSessionArtifact, EngineError> {
    let session = discover_with_roots(roots)
        .into_iter()
        .find(|session| {
            session.candidate.harness == HarnessId::Omp
                && session.candidate.session_id == native_session_id
                && session.candidate.cwd == cwd
        })
        .ok_or_else(|| EngineError::Other("local OMP session is no longer available".into()))?;
    capture_discovered_omp_artifact(&session, roots)
}

fn capture_discovered_omp_artifact(
    session: &DiscoveredSession,
    roots: &SessionRoots,
) -> Result<OmpSessionArtifact, EngineError> {
    if session.candidate.harness != HarnessId::Omp {
        return Err(EngineError::Other(
            "only OMP native sessions support exact artifact capture".into(),
        ));
    }
    let sessions_root = roots.omp.join("sessions");
    let storage_relative_path = session.path.strip_prefix(&sessions_root).map_err(|_| {
        EngineError::Other("OMP session path escaped the configured sessions root".into())
    })?;
    crate::omp_session_artifact::capture_omp_session_artifact(
        &session.path,
        storage_relative_path,
        &session.candidate.session_id,
        &session.candidate.cwd,
    )
}

/// Re-resolve an opaque candidate id, import exactly that transcript
/// idempotently, and return the Comet chat/space selected by the UI.
pub fn attach(
    candidate_id: &str,
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
) -> Result<LocalSessionAttachResult, EngineError> {
    let session = discover_with_roots(&session_roots())
        .into_iter()
        .find(|session| session.candidate.id == candidate_id)
        .ok_or_else(|| EngineError::Other("local session is no longer available".into()))?;
    let (attached, transcript_import) = materialize_discovered(&session, workspace, doc_host)?;
    apply_transcript_import(transcript_import, &session, workspace, doc_host)?;
    Ok(attached)
}

#[derive(Debug)]
struct SessionRepoMetadata {
    space_path: String,
    space_checkout_id: String,
    branch: String,
    checkout_id: String,
}

fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn checkout_id(device_id: &str, git_dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(device_id.as_bytes());
    hasher.update([0]);
    hasher.update(git_dir.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn session_repo_metadata(cwd: &Path, device_id: &str) -> Option<SessionRepoMetadata> {
    let branch = git_output(cwd, &["symbolic-ref", "--short", "HEAD"])
        .or_else(|| git_output(cwd, &["rev-parse", "--short", "HEAD"]))?;
    let git_dir = git_output(cwd, &["rev-parse", "--path-format=absolute", "--git-dir"])?;
    let common_dir = git_output(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let canonical_git_dir = fs::canonicalize(&git_dir).unwrap_or_else(|_| PathBuf::from(&git_dir));
    let canonical_common_dir =
        fs::canonicalize(&common_dir).unwrap_or_else(|_| PathBuf::from(&common_dir));
    let main_checkout = canonical_common_dir.parent()?;
    Some(SessionRepoMetadata {
        space_path: main_checkout.to_string_lossy().into_owned(),
        space_checkout_id: checkout_id(device_id, &canonical_common_dir),
        branch,
        checkout_id: checkout_id(device_id, &canonical_git_dir),
    })
}

fn local_space_id(path: &str) -> String {
    format!("local-space-{}", short_hash(path))
}

fn ensure_session_space(
    candidate: &LocalSessionCandidate,
    repo_metadata: Option<&SessionRepoMetadata>,
    workspace: &WorkspaceHost,
    device_id: &str,
) -> Result<String, EngineError> {
    let space_path = repo_metadata
        .map(|metadata| metadata.space_path.as_str())
        .unwrap_or(candidate.cwd.as_str());
    let spaces = workspace.read_spaces()?;
    let space_id = spaces
        .iter()
        .find(|space| space.device_id == device_id && space.path == space_path)
        .map(|space| space.id.clone())
        .unwrap_or_else(|| local_space_id(space_path));
    if !spaces.iter().any(|space| space.id == space_id) {
        workspace.create_space(
            &space_id,
            device_id,
            space_path,
            None,
            repo_metadata.is_some(),
        )?;
    }
    if let Some(metadata) = repo_metadata {
        workspace.set_space_git(&space_id, true, Some(&metadata.space_checkout_id))?;
    }
    Ok(space_id)
}

fn materialize_discovered(
    session: &DiscoveredSession,
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
) -> Result<(LocalSessionAttachResult, TranscriptImport), EngineError> {
    let candidate = &session.candidate;
    let device_id = doc_host.device_id();
    let repo_metadata = session_repo_metadata(Path::new(&candidate.cwd), device_id);
    let space_id = ensure_session_space(candidate, repo_metadata.as_ref(), workspace, device_id)?;

    let chat_id = candidate.chat_id.clone();
    let existing = workspace.doc().chat(&chat_id)?;
    let previous_space_id = existing
        .as_ref()
        .and_then(|chat| chat.space_id.as_ref())
        .filter(|id| id.as_str() != space_id)
        .cloned();
    let local_transcript_missing = existing
        .as_ref()
        .is_some_and(|chat| chat.harness_session_id.is_none())
        && doc_host
            .open(&chat_id)?
            .doc_arc()
            .read_entries()?
            .is_empty();
    let transcript_import = if existing.is_none() || local_transcript_missing {
        TranscriptImport::Full
    } else {
        existing
            .as_ref()
            .and_then(|chat| chat.last_message_at)
            .map(|at| at.timestamp_millis())
            .map_or(TranscriptImport::Full, |at| {
                if at < candidate.updated_at {
                    TranscriptImport::After(at)
                } else {
                    TranscriptImport::None
                }
            })
    };
    let config = ChatConfig {
        harness: if candidate.harness == HarnessId::OpenCode {
            HarnessId::Omp
        } else {
            candidate.harness
        },
        model: candidate.model.clone(),
        reasoning: candidate.reasoning,
        agent_account_id: None,
        model_options: serde_json::Map::new(),
        sandbox: SandboxLevel::WorkspaceWrite,
    };
    let expected_session_id = candidate.resumable.then_some(candidate.session_id.as_str());
    let metadata_matches = existing.as_ref().is_some_and(|chat| {
        chat.title.as_deref() == Some(candidate.title.as_str())
            && chat.cwd.as_deref() == Some(candidate.cwd.as_str())
            && chat.space_id.as_deref() == Some(space_id.as_str())
            && chat.config.as_ref() == Some(&config)
            && chat.harness_session_id.as_deref() == expected_session_id
            && repo_metadata.as_ref().is_none_or(|metadata| {
                chat.branch.as_deref() == Some(metadata.branch.as_str())
                    && chat.checkout_id.as_deref() == Some(metadata.checkout_id.as_str())
            })
    });
    // This path is an explicit authenticated attach of an opaque candidate,
    // not a shared-room import. Pin before the no-change fast path so a user
    // who selects a pre-membership native session keeps it after the upgrade.
    workspace.upsert_session_ref(&chat_id, None)?;
    if metadata_matches && transcript_import == TranscriptImport::None {
        return Ok((
            LocalSessionAttachResult { chat_id, space_id },
            TranscriptImport::None,
        ));
    }

    if existing.is_none() {
        workspace.create_chat(
            &chat_id,
            &space_id,
            Some(config.clone()),
            Some(candidate.cwd.clone()),
        )?;
        workspace.set_chat_activity(&chat_id, None, Some(candidate.created_at))?;
    }
    if let Some(metadata) = repo_metadata.as_ref() {
        workspace.set_chat_branch(&chat_id, &metadata.branch)?;
        workspace.set_chat_checkout(&chat_id, &metadata.checkout_id)?;
    }
    if candidate.resumable {
        workspace.set_chat_harness_session(&chat_id, &candidate.session_id, &candidate.cwd);
    }
    if existing
        .as_ref()
        .is_some_and(|chat| chat.cwd.as_deref() != Some(candidate.cwd.as_str()))
    {
        workspace.set_chat_cwd(&chat_id, &candidate.cwd)?;
    }
    if existing
        .as_ref()
        .is_some_and(|chat| chat.space_id.as_deref() != Some(space_id.as_str()))
    {
        workspace.set_chat_space(&chat_id, &space_id)?;
    }
    if let Some(previous_space_id) = previous_space_id {
        let still_used = workspace
            .doc()
            .read_chats()?
            .iter()
            .any(|chat| chat.space_id.as_deref() == Some(previous_space_id.as_str()));
        if !still_used {
            let obsolete = workspace
                .read_spaces()?
                .into_iter()
                .find(|space| space.id == previous_space_id);
            if obsolete.as_ref().is_some_and(|space| {
                space.device_id == device_id && space.id == local_space_id(&space.path)
            }) {
                workspace.delete_space(&previous_space_id)?;
            }
        }
    }
    if existing
        .as_ref()
        .is_none_or(|chat| chat.title.as_deref() != Some(candidate.title.as_str()))
    {
        workspace.rename_chat(&chat_id, &candidate.title)?;
    }
    if existing
        .as_ref()
        .is_none_or(|chat| chat.config.as_ref() != Some(&config))
    {
        workspace.set_chat_config(&chat_id, &config)?;
    }
    Ok((
        LocalSessionAttachResult { chat_id, space_id },
        transcript_import,
    ))
}

fn apply_transcript_import(
    transcript_import: TranscriptImport,
    session: &DiscoveredSession,
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
) -> Result<(), EngineError> {
    match transcript_import {
        TranscriptImport::None => Ok(()),
        TranscriptImport::Full => import_transcript(session, workspace, doc_host, None),
        TranscriptImport::After(cutoff) => {
            import_transcript(session, workspace, doc_host, Some(cutoff))
        }
    }
}

fn import_transcript(
    session: &DiscoveredSession,
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
    imported_after: Option<i64>,
) -> Result<(), EngineError> {
    let candidate = &session.candidate;
    let entries = load_transcript(session, doc_host.device_id())?;
    let doc = doc_host.open(&candidate.chat_id)?;
    let session_doc = doc.doc_arc();
    let existing_ids = session_doc.message_ids();
    let new_entries: Vec<_> = entries
        .iter()
        .filter(|entry| !imported_after.is_some_and(|cutoff| entry.created_at <= cutoff))
        .filter(|entry| !existing_ids.contains(&entry.id))
        .cloned()
        .collect();
    session_doc.push_messages(&new_entries)?;
    if let Some(preview) = entries
        .last()
        .and_then(|entry| entry.parts.iter().find_map(text_part))
        .or(candidate.preview.as_deref())
    {
        workspace.note_message(&candidate.chat_id, preview);
    }
    workspace.set_chat_activity(
        &candidate.chat_id,
        Some(candidate.updated_at),
        Some(candidate.created_at),
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
struct SessionRoots {
    claude: PathBuf,
    codex: PathBuf,
    omp: PathBuf,
    prime: PathBuf,
    prime_sessions: PathBuf,
    opencode: PathBuf,
}

fn session_roots() -> SessionRoots {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let xdg_data = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let pi_agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let prime_sessions = [
        "PRIME_AGENT_SESSION_DIR",
        "PI_SESSION_DIR",
        "PI_CODING_AGENT_SESSION_DIR",
    ]
    .into_iter()
    .find_map(|name| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
    .unwrap_or_else(|| {
        pi_agent_dir
            .as_ref()
            .cloned()
            .unwrap_or_else(|| home.join(".pi/agent"))
            .join("sessions")
    });
    SessionRoots {
        claude: std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude")),
        codex: std::env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex")),
        omp: pi_agent_dir.unwrap_or_else(|| home.join(".omp/agent")),
        prime: std::env::var_os("PRIME_AGENT_CODING_AGENT_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".prime/agent")),
        prime_sessions,
        opencode: xdg_data.join("opencode/opencode.db"),
    }
}

fn discover_with_roots(roots: &SessionRoots) -> Vec<DiscoveredSession> {
    let mut sessions = std::thread::scope(|scope| {
        let claude = scope.spawn(|| discover_claude(&roots.claude));
        let codex = scope.spawn(|| discover_codex(&roots.codex));
        let omp = scope.spawn(|| discover_omp(&roots.omp));
        let prime = scope.spawn(|| discover_prime_agent(&roots.prime, &roots.prime_sessions));
        let opencode = scope.spawn(|| discover_opencode(&roots.opencode));
        let mut sessions = Vec::new();
        sessions.extend(claude.join().expect("Claude session discovery panicked"));
        sessions.extend(codex.join().expect("Codex session discovery panicked"));
        sessions.extend(omp.join().expect("OMP session discovery panicked"));
        sessions.extend(
            prime
                .join()
                .expect("Prime Agent session discovery panicked"),
        );
        sessions.extend(
            opencode
                .join()
                .expect("OpenCode session discovery panicked"),
        );
        sessions
    });
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
                build_session(
                    HarnessId::Codex,
                    session_id,
                    cwd,
                    Some(title),
                    None,
                    model,
                    reasoning.as_deref().and_then(parse_reasoning),
                    Some(normalize_epoch(created_at)),
                    Some(normalize_epoch(updated_at)),
                    &PathBuf::from(rollout_path),
                )
            },
        )
        .collect()
}
fn discover_opencode(path: &Path) -> Vec<DiscoveredSession> {
    let Ok(connection) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "SELECT id, directory, title, model, time_created, time_updated \
         FROM session \
         WHERE time_archived IS NULL AND (parent_id IS NULL OR parent_id = '') \
         ORDER BY time_updated DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([MAX_CANDIDATES_PER_HARNESS as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
        ))
    }) else {
        return Vec::new();
    };
    rows.filter_map(Result::ok)
        .filter_map(|(session_id, cwd, title, model, created_at, updated_at)| {
            build_session(
                HarnessId::OpenCode,
                session_id,
                cwd,
                title,
                None,
                model.as_deref().and_then(opencode_model),
                None,
                Some(normalize_epoch(created_at)),
                Some(normalize_epoch(updated_at)),
                path,
            )
        })
        .collect()
}

fn opencode_model(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    if let Some(model) = value.as_str().filter(|model| !model.trim().is_empty()) {
        return Some(model.to_string());
    }
    let model = ["id", "modelID", "modelId", "model_id", "model"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .filter(|model| !model.trim().is_empty())?;
    let provider = ["providerID", "providerId", "provider_id", "provider"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .filter(|provider| !provider.trim().is_empty());
    Some(match provider {
        Some(provider) => format!("{provider}/{model}"),
        None => model.to_string(),
    })
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
    // Parse bounded headers before probing ownership. Most OMP journals are
    // internal advisor/worker shells with no user turn, so they never enter
    // the single batched descriptor scan.
    let mut sessions: Vec<_> =
        recent_omp_jsonl_files(&root.join("sessions"), MAX_CANDIDATES_PER_HARNESS)
            .into_iter()
            .filter_map(|path| {
                let session = candidate_from_omp_with_writer_state(
                    &path,
                    comet_harness::omp::SessionWriterState::Unknown,
                )?;
                session
                    .candidate
                    .preview
                    .as_deref()
                    .filter(|preview| !preview.trim().is_empty())?;
                Some(session)
            })
            .collect();
    let paths: Vec<_> = sessions
        .iter()
        .map(|session| session.path.clone())
        .collect();
    let writer_states = comet_harness::omp::session_writer_states(&paths);
    for (session, writer_state) in sessions.iter_mut().zip(writer_states) {
        apply_omp_writer_state(session, writer_state);
    }
    sessions
}

/// Latest native journal activity for a specific OMP session. A Comet-owned
/// turn records this watermark at `Done`, after the ACP response is durable,
/// so later history refreshes start strictly after records Comet already
/// rendered through the live stream.
///
/// This completion path only needs persisted metadata. Writer ownership is
/// intentionally excluded: probing it can launch `lsof`, which must never
/// block the session engine after a turn finishes.
pub(crate) fn omp_session_updated_at(session_id: &str, cwd: &str) -> Option<i64> {
    omp_session_updated_at_with_root(&session_roots().omp, session_id, cwd)
}

fn omp_session_updated_at_with_root(root: &Path, session_id: &str, cwd: &str) -> Option<i64> {
    recent_omp_jsonl_files(&root.join("sessions"), MAX_CANDIDATES_PER_HARNESS)
        .into_iter()
        .filter_map(|path| {
            candidate_from_omp_with_writer_state(
                &path,
                comet_harness::omp::SessionWriterState::Unknown,
            )
        })
        .find(|session| session.candidate.session_id == session_id && session.candidate.cwd == cwd)
        .map(|session| session.candidate.updated_at)
}

#[cfg(test)]
fn candidate_from_omp(path: &Path) -> Option<DiscoveredSession> {
    candidate_from_omp_with_writer_state(path, comet_harness::omp::session_writer_state(path))
}

fn canonical_omp_model_selector(model: &str) -> Option<String> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    for (internal, public) in [
        ("comet-openai/", "openai-codex/"),
        ("scaffold-openai/", "openai-codex/"),
        ("comet-anthropic/", "anthropic/"),
        ("scaffold-anthropic/", "anthropic/"),
    ] {
        if let Some(id) = model.strip_prefix(internal).filter(|id| !id.is_empty()) {
            return Some(format!("{public}{id}"));
        }
    }
    Some(model.to_string())
}

fn candidate_from_omp_with_writer_state(
    path: &Path,
    writer_state: comet_harness::omp::SessionWriterState,
) -> Option<DiscoveredSession> {
    let values = read_prefix_values(path);
    let mut session_id = None;
    let mut cwd = None;
    let mut title = None;
    let mut has_title_record = false;
    let mut preview = None;
    let mut model = None;
    let mut reasoning = None;
    let mut created_at = None;
    let mut updated_at = file_modified_ms(path);

    for value in values {
        match string_at(&value, &["type"]) {
            Some("session") => {
                if value.get("rlmDepth").is_some() {
                    return None;
                }
                session_id = string_at(&value, &["id"])
                    .map(str::to_string)
                    .or(session_id);
                cwd = string_at(&value, &["cwd"]).map(str::to_string).or(cwd);
                if !has_title_record {
                    title = string_at(&value, &["title"]).map(str::to_string).or(title);
                }
            }
            Some("title") => {
                if let Some(value) = string_at(&value, &["title"])
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    title = Some(value.to_string());
                    has_title_record = true;
                }
            }
            Some("custom-title") => {
                if let Some(value) = string_at(&value, &["customTitle"])
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    title = Some(value.to_string());
                    has_title_record = true;
                }
            }
            Some("model_change") => {
                model = string_at(&value, &["model"])
                    .and_then(canonical_omp_model_selector)
                    .or(model);
            }
            Some("thinking_level_change") => {
                reasoning = string_at(&value, &["thinkingLevel"])
                    .and_then(parse_reasoning)
                    .or(reasoning);
            }
            Some("message") => {
                let message = value.get("message");
                if string_at_opt(message, &["role"]) == Some("user") {
                    preview = preview.or_else(|| message.and_then(message_text).map(short_text));
                }
                model = model.or_else(|| {
                    string_at_opt(message, &["model"])
                        .or_else(|| string_at(&value, &["model"]))
                        .and_then(canonical_omp_model_selector)
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
    let mut session = build_session(
        HarnessId::Omp,
        session_id,
        cwd?,
        title,
        preview,
        model,
        reasoning,
        created_at,
        updated_at,
        path,
    )?;
    apply_omp_writer_state(&mut session, writer_state);
    Some(session)
}
fn apply_omp_writer_state(
    session: &mut DiscoveredSession,
    writer_state: comet_harness::omp::SessionWriterState,
) {
    match writer_state {
        comet_harness::omp::SessionWriterState::Active => {
            session.candidate.resumable = false;
            session.candidate.history_only = true;
            session.candidate.busy_elsewhere = Some(true);
        }
        comet_harness::omp::SessionWriterState::Inactive => {
            session.candidate.resumable = true;
            session.candidate.history_only = false;
            session.candidate.busy_elsewhere = Some(false);
        }
        comet_harness::omp::SessionWriterState::Unknown => {
            session.candidate.resumable = false;
            session.candidate.history_only = true;
            session.candidate.busy_elsewhere = None;
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrimeLiveMetadata {
    root_session_id: Option<String>,
    session_file: Option<PathBuf>,
    created_at: Option<String>,
    updated_at: Option<String>,
    lifecycle: Option<String>,
    create_command: Option<PrimeLiveCreateCommand>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrimeLiveCreateCommand {
    session_path: Option<PathBuf>,
    config: Option<PrimeLiveConfig>,
}

#[derive(Debug, Deserialize)]
struct PrimeLiveConfig {
    cwd: Option<String>,
}

fn discover_prime_agent(root: &Path, current_sessions: &Path) -> Vec<DiscoveredSession> {
    let session_roots = [
        root.join("sessions"),
        root.join("comet-sessions"),
        current_sessions.to_path_buf(),
    ];
    let mut seen_paths = HashSet::new();
    let mut files = Vec::new();
    for session_root in &session_roots {
        for path in recent_jsonl_files(session_root, MAX_CANDIDATES_PER_HARNESS) {
            if seen_paths.insert(path.clone()) {
                files.push(path);
            }
        }
    }
    files.sort_by_key(|path| std::cmp::Reverse(file_modified_ms(path).unwrap_or_default()));

    let mut sessions = Vec::<DiscoveredSession>::new();
    for session in files
        .into_iter()
        .filter_map(|path| candidate_from_prime_agent(&path))
    {
        if let Some(existing) = sessions
            .iter_mut()
            .find(|existing| existing.candidate.session_id == session.candidate.session_id)
        {
            if session.candidate.updated_at > existing.candidate.updated_at {
                *existing = session;
            }
        } else {
            sessions.push(session);
        }
    }

    for metadata_path in recent_prime_live_metadata_files(root) {
        let Some(metadata) = read_prime_live_metadata(&metadata_path) else {
            continue;
        };
        if matches!(
            metadata.lifecycle.as_deref(),
            Some("stopped" | "exited" | "failed" | "terminated")
        ) {
            continue;
        }
        let metadata_updated_at = parse_timestamp(metadata.updated_at.as_deref())
            .or_else(|| file_modified_ms(&metadata_path));
        let durable_path = metadata
            .session_file
            .as_deref()
            .or_else(|| {
                metadata
                    .create_command
                    .as_ref()
                    .and_then(|command| command.session_path.as_deref())
            })
            .and_then(|path| validated_prime_session_path(path, &session_roots));
        let mut live_session = durable_path.as_deref().and_then(candidate_from_prime_agent);
        if let Some(session) = live_session.as_mut() {
            session.candidate.updated_at = session
                .candidate
                .updated_at
                .max(metadata_updated_at.unwrap_or_default());
            session.candidate.busy_elsewhere = Some(true);
        }

        let live_session = live_session.or_else(|| {
            let session_id = metadata
                .root_session_id
                .filter(|session_id| !session_id.trim().is_empty())?;
            let cwd = metadata
                .create_command
                .as_ref()
                .and_then(|command| command.config.as_ref())
                .and_then(|config| config.cwd.clone())
                .filter(|cwd| !cwd.trim().is_empty())?;
            let mut session = build_session(
                HarnessId::PrimeAgent,
                session_id,
                cwd,
                None,
                None,
                None,
                None,
                parse_timestamp(metadata.created_at.as_deref()),
                metadata_updated_at,
                &metadata_path,
            )?;
            session.candidate.resumable = false;
            session.candidate.history_only = false;
            session.candidate.busy_elsewhere = Some(true);
            Some(session)
        });
        let Some(live_session) = live_session else {
            continue;
        };

        if let Some(existing) = sessions
            .iter_mut()
            .find(|existing| existing.candidate.session_id == live_session.candidate.session_id)
        {
            existing.candidate.updated_at = existing
                .candidate
                .updated_at
                .max(live_session.candidate.updated_at);
            existing.candidate.busy_elsewhere = Some(true);
        } else {
            sessions.push(live_session);
        }
    }

    sessions.sort_by(|a, b| {
        b.candidate
            .updated_at
            .cmp(&a.candidate.updated_at)
            .then_with(|| a.candidate.id.cmp(&b.candidate.id))
    });
    sessions.truncate(MAX_CANDIDATES_PER_HARNESS);
    sessions
}

fn candidate_from_prime_agent(path: &Path) -> Option<DiscoveredSession> {
    let values = read_prefix_values(path);
    let mut session_id = None;
    let mut cwd = None;
    let mut title = None;
    let mut preview = None;
    let mut model = None;
    let mut reasoning = None;
    let mut created_at = None;
    let mut updated_at = file_modified_ms(path);
    let mut is_root_session = false;

    for value in values {
        match string_at(&value, &["type"]) {
            Some("session") => {
                let depth = value.get("rlmDepth").and_then(Value::as_u64)?;
                if depth > 0 {
                    return None;
                }
                is_root_session = true;
                session_id = string_at(&value, &["id"])
                    .map(str::to_string)
                    .or(session_id);
                cwd = string_at(&value, &["cwd"]).map(str::to_string).or(cwd);
                title = string_at(&value, &["sessionName"])
                    .or_else(|| string_at(&value, &["name"]))
                    .map(str::to_string)
                    .or(title);
            }
            Some("model_change") => {
                if let Some(model_id) = string_at(&value, &["model"]) {
                    model = Some(
                        string_at(&value, &["provider"])
                            .map(|provider| format!("{provider}/{model_id}"))
                            .unwrap_or_else(|| model_id.to_string()),
                    );
                }
            }
            Some("thinking_level_change") => {
                reasoning = string_at(&value, &["thinkingLevel"])
                    .and_then(parse_reasoning)
                    .or(reasoning);
            }
            Some("message") => {
                let message = value.get("message");
                if string_at_opt(message, &["role"]) == Some("user") {
                    preview = preview.or_else(|| message.and_then(message_text).map(short_text));
                }
            }
            _ => {}
        }
        if let Some(ms) = parse_timestamp(string_at(&value, &["timestamp"])) {
            created_at = Some(created_at.map_or(ms, |old: i64| old.min(ms)));
            updated_at = Some(updated_at.map_or(ms, |old| old.max(ms)));
        }
    }

    if !is_root_session {
        return None;
    }
    let session_id = session_id.or_else(|| path.file_stem()?.to_str().map(str::to_string))?;
    build_session(
        HarnessId::PrimeAgent,
        session_id,
        cwd?,
        title,
        preview,
        model,
        reasoning,
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
    let chat_id = if harness == HarnessId::OpenCode {
        format!("local-chat-opencode-{}", short_hash(&id))
    } else {
        format!("local-chat-{}", short_hash(&id))
    };

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
            live_attachable: false,
            resumable: matches!(
                harness,
                HarnessId::ClaudeCode | HarnessId::Codex | HarnessId::Omp | HarnessId::PrimeAgent
            ),
            history_only: harness == HarnessId::OpenCode,
            busy_elsewhere: None,
        },
        path: path.to_path_buf(),
    })
}

fn load_transcript(
    session: &DiscoveredSession,
    device_id: &str,
) -> Result<Vec<SessionMessageEntry>, EngineError> {
    if session.candidate.harness == HarnessId::OpenCode {
        return load_opencode_transcript(session, device_id);
    }
    let active_omp_records = matches!(
        session.candidate.harness,
        HarnessId::Omp | HarnessId::PrimeAgent
    )
    .then(|| active_omp_record_ids(&session.path))
    .transpose()?
    .flatten();
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
            HarnessId::PrimeAgent => transcript_record_omp(&value),
            _ => None,
        };
        let Some((source_id, role, text, timestamp)) = extracted else {
            continue;
        };
        if active_omp_records.as_ref().is_some_and(|active| {
            source_id
                .as_deref()
                .is_none_or(|source_id| !active.contains(source_id))
        }) {
            continue;
        }
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

/// Return the ids on the journal's current parent-linked branch. Older OMP
/// fixtures and legacy journals without parent links return `None`, preserving
/// their append-order import behavior.
fn active_omp_record_ids(path: &Path) -> Result<Option<HashSet<String>>, EngineError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut parents = HashMap::<String, Option<String>>::new();
    let mut latest_linked_id = None;
    let mut retained_id_bytes = 0_usize;

    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.len() > MAX_JSONL_RECORD_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let Some(id) = string_at(&value, &["id"]) else {
            continue;
        };
        let parent = string_at(&value, &["parentId"]).map(str::to_string);
        if parent.is_some() {
            latest_linked_id = Some(id.to_string());
        }
        retained_id_bytes = retained_id_bytes
            .saturating_add(id.len())
            .saturating_add(parent.as_deref().map_or(0, str::len));
        if parents.len() >= MAX_TRANSCRIPT_GRAPH_RECORDS
            || retained_id_bytes > MAX_TRANSCRIPT_GRAPH_ID_BYTES
        {
            return Err(EngineError::Other(
                "native session branch graph exceeds the import limit".into(),
            ));
        }
        parents.insert(id.to_string(), parent);
    }

    let Some(mut cursor) = latest_linked_id else {
        return Ok(None);
    };
    let mut active = HashSet::new();
    loop {
        if !active.insert(cursor.clone()) {
            return Err(EngineError::Other(
                "native session branch graph contains a cycle".into(),
            ));
        }
        let Some(parent) = parents.get(&cursor).cloned().flatten() else {
            break;
        };
        cursor = parent;
    }
    Ok(Some(active))
}
fn opencode_store_error(error: rusqlite::Error) -> EngineError {
    EngineError::Other(format!("OpenCode session store: {error}"))
}

fn load_opencode_transcript(
    session: &DiscoveredSession,
    device_id: &str,
) -> Result<Vec<SessionMessageEntry>, EngineError> {
    let connection = Connection::open_with_flags(
        &session.path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(opencode_store_error)?;
    let mut statement = connection
        .prepare(
            "SELECT m.id, m.time_created, m.data, p.data \
             FROM message AS m \
             JOIN part AS p ON p.message_id = m.id AND p.session_id = m.session_id \
             WHERE m.session_id = ?1 \
             ORDER BY m.time_created, m.id, p.time_created, p.rowid",
        )
        .map_err(opencode_store_error)?;
    let rows = statement
        .query_map([session.candidate.session_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(opencode_store_error)?;
    let mut entries: Vec<SessionMessageEntry> = Vec::new();
    let mut last_source_id: Option<String> = None;
    for row in rows {
        let (source_id, created_at, message_data, part_data) = row.map_err(opencode_store_error)?;
        let Ok(message) = serde_json::from_str::<Value>(&message_data) else {
            continue;
        };
        let Some(role) = string_at(&message, &["role"]).and_then(parse_role) else {
            continue;
        };
        let Ok(part) = serde_json::from_str::<Value>(&part_data) else {
            continue;
        };
        if string_at(&part, &["type"]) != Some("text") {
            continue;
        }
        let Some(text) = string_at(&part, &["text"]).filter(|text| !text.trim().is_empty()) else {
            continue;
        };

        if last_source_id.as_deref() != Some(source_id.as_str()) {
            let entry_id = format!(
                "local-import-{}",
                short_hash(&format!("{}:{source_id}", session.candidate.id))
            );
            entries.push(SessionMessageEntry {
                id: entry_id,
                role,
                parts: Vec::new(),
                created_at: normalize_epoch(created_at),
                device_id: device_id.to_string(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
            });
            last_source_id = Some(source_id);
        }
        let entry = entries
            .last_mut()
            .expect("OpenCode text part always creates an entry");
        let part_id = format!("{}-text-{}", entry.id, entry.parts.len());
        entry.parts.push(MessagePart::Text {
            id: part_id,
            text: text.to_string(),
        });
    }
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
        MessagePart::Text { text, .. } | MessagePart::TextWindow { text, .. } => {
            Some(text.as_str())
        }
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
fn read_prime_live_metadata(path: &Path) -> Option<PrimeLiveMetadata> {
    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(PREFIX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > PREFIX_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn recent_omp_jsonl_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    fn retain_recent_jsonl_files(
        path: &Path,
        limit: usize,
        files: &mut BinaryHeap<Reverse<(i64, PathBuf)>>,
    ) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            {
                files.push(Reverse((file_modified_ms(&path).unwrap_or_default(), path)));
                if files.len() > limit {
                    files.pop();
                }
            }
        }
    }

    if limit == 0 {
        return Vec::new();
    }
    let mut files = BinaryHeap::with_capacity(limit.saturating_add(1));
    retain_recent_jsonl_files(root, limit, &mut files);
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            retain_recent_jsonl_files(&entry.path(), limit, &mut files);
        }
    }
    let mut files: Vec<_> = files
        .into_iter()
        .map(|Reverse((modified, path))| (modified, path))
        .collect();
    files.sort_by_key(|(modified, path)| (Reverse(*modified), path.clone()));
    files.into_iter().map(|(_, path)| path).collect()
}

fn recent_prime_live_metadata_files(root: &Path) -> Vec<PathBuf> {
    let workers_root = root.join("daemon-workers");
    let Ok(supervisors) = fs::read_dir(&workers_root) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for supervisor in supervisors.flatten() {
        if !supervisor
            .file_type()
            .is_ok_and(|file_type| file_type.is_dir())
        {
            continue;
        }
        let Ok(entries) = fs::read_dir(supervisor.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|file_type| file_type.is_file())
                && path.extension().and_then(|ext| ext.to_str()) == Some("json")
            {
                files.push((file_modified_ms(&path).unwrap_or_default(), path));
            }
        }
    }
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files.truncate(MAX_CANDIDATES_PER_HARNESS);
    files.into_iter().map(|(_, path)| path).collect()
}

fn validated_prime_session_path(path: &Path, session_roots: &[PathBuf]) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let canonical_path = fs::canonicalize(path).ok()?;
    if !canonical_path.is_file() {
        return None;
    }
    session_roots.iter().find_map(|root| {
        let canonical_root = fs::canonicalize(root).ok()?;
        canonical_path
            .starts_with(&canonical_root)
            .then(|| canonical_path.clone())
    })
}

fn recent_jsonl_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    fn visit(path: &Path, depth: usize, files: &mut Vec<(i64, PathBuf)>) {
        if depth > MAX_DISCOVERY_DEPTH {
            return;
        }
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                visit(&path, depth + 1, files);
            } else if file_type.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            {
                files.push((file_modified_ms(&path).unwrap_or_default(), path));
            }
        }
    }

    let mut files = Vec::new();
    visit(root, 0, &mut files);
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
        HarnessId::PrimeAgent => "prime-agent",
        HarnessId::OpenCode => "opencode",
        HarnessId::Cursor => "cursor",
        HarnessId::Mock => "mock",
    }
}

fn harness_label(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Claude Code",
        HarnessId::Codex => "Codex",
        HarnessId::Omp => "OMP",
        HarnessId::PrimeAgent => "Prime Agent",
        HarnessId::OpenCode => "OpenCode",
        HarnessId::Cursor => "Cursor",
        HarnessId::Mock => "Test",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;

    use comet_sync::DocsStore;
    use tempfile::TempDir;

    use crate::doc_host::DocHostConfig;
    use crate::workspace_host::WorkspaceHostConfig;

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

    fn opencode_fixture(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    title TEXT,
                    model TEXT,
                    time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL,
                    time_archived INTEGER,
                    parent_id TEXT
                );
                CREATE TABLE message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL,
                    data TEXT NOT NULL
                );
                CREATE TABLE part (
                    message_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL,
                    data TEXT NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session
                 (id, directory, title, model, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    "opencode-1",
                    "/repo",
                    "OpenCode review",
                    r#"{"id":"gpt-5.6-sol","providerID":"cliproxy","variant":"default"}"#,
                    1_754_389_000_000_i64,
                    1_754_389_060_000_i64
                ],
            )
            .unwrap();
        for (id, role, text, created_at) in [
            (
                "",
                "user",
                "Inspect the database history",
                1_754_389_000_000_i64,
            ),
            (
                "oc-a1",
                "assistant",
                "The history is intact",
                1_754_389_060_000_i64,
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO message (id, session_id, time_created, data)
                     VALUES (?1, 'opencode-1', ?2, ?3)",
                    rusqlite::params![
                        id,
                        created_at,
                        serde_json::json!({"role": role}).to_string()
                    ],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO part (message_id, session_id, time_created, data)
                     VALUES (?1, 'opencode-1', ?2, ?3)",
                    rusqlite::params![
                        id,
                        created_at,
                        serde_json::json!({"type": "text", "text": text}).to_string()
                    ],
                )
                .unwrap();
        }
    }

    #[test]
    fn discovers_all_five_native_session_formats() {
        let temp = TempDir::new().unwrap();
        let roots = SessionRoots {
            claude: temp.path().join("claude"),
            codex: temp.path().join("codex"),
            omp: temp.path().join("omp"),
            prime: temp.path().join("prime"),
            prime_sessions: temp.path().join("pi/sessions"),
            opencode: temp.path().join("opencode.db"),
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
        fixture(
            &roots.prime,
            "sessions/prime/prime-1.jsonl",
            &[
                serde_json::json!({"type":"session","id":"prime-1","cwd":"/repo","rlmDepth":0,"timestamp":"2026-08-05T13:00:00Z"}),
                serde_json::json!({"type":"model_change","provider":"openai-codex","model":"gpt-5.6-sol"}),
                serde_json::json!({"type":"thinking_level_change","thinkingLevel":"high"}),
                serde_json::json!({"type":"message","id":"p1","timestamp":"2026-08-05T13:01:00Z","message":{"role":"user","content":[{"type":"text","text":"Audit the workflow"}]}}),
            ],
        );
        opencode_fixture(&roots.opencode);

        let sessions = discover_with_roots(&roots);
        assert_eq!(sessions.len(), 5);
        assert!(sessions.iter().all(|session| {
            let ownership_is_safe = if session.candidate.harness == HarnessId::Omp {
                match session.candidate.busy_elsewhere {
                    Some(true) => !session.candidate.resumable,
                    Some(false) => session.candidate.resumable,
                    None => !session.candidate.resumable,
                }
            } else {
                session.candidate.busy_elsewhere.is_none()
            };
            !session.candidate.live_attachable
                && ownership_is_safe
                && (session.candidate.harness != HarnessId::OpenCode
                    && session.candidate.harness != HarnessId::Omp
                    || session.candidate.harness == HarnessId::Omp
                        && session.candidate.busy_elsewhere == Some(false))
                    == session.candidate.resumable
                && (session.candidate.harness == HarnessId::OpenCode)
                    == session.candidate.history_only
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
        assert!(sessions.iter().any(|session| {
            session.candidate.harness == HarnessId::PrimeAgent
                && session.candidate.title == "Audit the workflow"
                && session.candidate.model.as_deref() == Some("openai-codex/gpt-5.6-sol")
                && session.candidate.reasoning == Some(ReasoningLevel::High)
        }));
        assert!(sessions.iter().any(|session| {
            session.candidate.harness == HarnessId::OpenCode
                && session.candidate.title == "OpenCode review"
                && session.candidate.model.as_deref() == Some("cliproxy/gpt-5.6-sol")
                && session
                    .candidate
                    .chat_id
                    .starts_with("local-chat-opencode-")
                && session.candidate.history_only
                && !session.candidate.resumable
        }));
    }

    #[test]
    fn codex_database_listing_uses_indexed_metadata_without_parsing_rollout() {
        let temp = TempDir::new().unwrap();
        let rollout = temp.path().join("rollout.jsonl");
        fs::write(&rollout, "{not valid json").unwrap();
        let database = temp.path().join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT NOT NULL,
                    rollout_path TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    cwd TEXT NOT NULL,
                    title TEXT NOT NULL,
                    model TEXT,
                    reasoning_effort TEXT,
                    archived INTEGER NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (
                    id, rollout_path, created_at, updated_at, cwd, title,
                    model, reasoning_effort, archived
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
                rusqlite::params![
                    "codex-indexed",
                    rollout.to_string_lossy(),
                    1_786_000_000_i64,
                    1_786_000_100_i64,
                    "/repo",
                    "Indexed title",
                    "gpt-5.6-sol",
                    "high",
                ],
            )
            .unwrap();
        drop(connection);

        let sessions = discover_codex_db(&database);
        assert_eq!(sessions.len(), 1);
        let candidate = &sessions[0].candidate;
        assert_eq!(candidate.session_id, "codex-indexed");
        assert_eq!(candidate.title, "Indexed title");
        assert_eq!(candidate.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(candidate.reasoning, Some(ReasoningLevel::High));
        assert_eq!(candidate.preview, None);
    }

    #[tokio::test]
    async fn imports_opencode_transcript_on_attach_with_omp_fork_config() {
        let temp = TempDir::new().unwrap();
        let database = temp.path().join("opencode.db");
        opencode_fixture(&database);
        let session = discover_opencode(&database).pop().unwrap();
        let first = load_opencode_transcript(&session, "device-a").unwrap();
        let second = load_opencode_transcript(&session, "device-a").unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].role, MessageRole::User);
        assert_eq!(first[1].role, MessageRole::Assistant);
        assert_eq!(
            text_part(&first[0].parts[0]),
            Some("Inspect the database history")
        );

        let store = Arc::new(DocsStore::open(temp.path().join("docs")).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            WorkspaceHostConfig {
                device_id: "device-a".into(),
                device_name: "Test Mac".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "user-a".into(),
                edge: None,
            },
        )
        .unwrap();
        let doc_host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "device-a".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        doc_host.set_workspace(workspace.clone());

        let (attached, transcript_import) =
            materialize_discovered(&session, &workspace, &doc_host).unwrap();
        assert_eq!(transcript_import, TranscriptImport::Full);
        apply_transcript_import(transcript_import, &session, &workspace, &doc_host).unwrap();
        let chat = workspace.doc().chat(&attached.chat_id).unwrap().unwrap();
        assert_eq!(
            chat.config.as_ref().map(|config| config.harness),
            Some(HarnessId::Omp)
        );
        assert!(chat.harness_session_id.is_none());
        assert_eq!(
            doc_host
                .open(&attached.chat_id)
                .unwrap()
                .doc_arc()
                .read_entries()
                .unwrap(),
            first
        );
    }

    #[tokio::test]
    async fn materializes_native_history_as_an_idempotent_session() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let session_path = fixture(
            temp.path(),
            "omp/sessions/by-cwd/omp-1.jsonl",
            &[
                serde_json::json!({
                    "type": "session",
                    "id": "omp-1",
                    "cwd": repo,
                    "timestamp": "2026-08-05T12:00:00Z"
                }),
                serde_json::json!({
                    "type": "model_change",
                    "model": "comet-openai/gpt-5.6-sol"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "m1",
                    "message": {"role": "user", "content": "Plan the staging restart"}
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "m2",
                    "message": {
                        "role": "assistant",
                        "model": "comet-openai/gpt-5.6-sol",
                        "content": "Here is the plan"
                    }
                }),
            ],
        );
        let session = candidate_from_omp(&session_path).unwrap();
        let store = Arc::new(DocsStore::open(temp.path().join("docs")).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            WorkspaceHostConfig {
                device_id: "device-a".into(),
                device_name: "Test Mac".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "user-a".into(),
                edge: None,
            },
        )
        .unwrap();
        let doc_host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "device-a".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        doc_host.set_workspace(workspace.clone());

        let (attached, transcript_import) =
            materialize_discovered(&session, &workspace, &doc_host).unwrap();
        assert_eq!(transcript_import, TranscriptImport::Full);
        apply_transcript_import(transcript_import, &session, &workspace, &doc_host).unwrap();

        let chat = workspace
            .doc()
            .read_chats()
            .unwrap()
            .into_iter()
            .find(|chat| chat.id == attached.chat_id)
            .unwrap();
        assert_eq!(chat.title.as_deref(), Some("Plan the staging restart"));
        assert_eq!(chat.cwd.as_deref(), Some(repo.to_string_lossy().as_ref()));
        assert_eq!(
            chat.config.as_ref().map(|config| config.harness),
            Some(HarnessId::Omp)
        );
        assert_eq!(
            chat.config
                .as_ref()
                .and_then(|config| config.model.as_deref()),
            Some("openai-codex/gpt-5.6-sol")
        );
        assert_eq!(
            doc_host
                .open(&attached.chat_id)
                .unwrap()
                .doc_arc()
                .read_entries()
                .unwrap()
                .len(),
            2
        );

        workspace
            .doc()
            .remove_session_ref("user-a", &attached.chat_id)
            .unwrap();
        assert!(
            workspace
                .doc()
                .session_ref("user-a", &attached.chat_id)
                .unwrap()
                .is_none()
        );

        let (reattached, transcript_import) =
            materialize_discovered(&session, &workspace, &doc_host).unwrap();
        assert_eq!(reattached.chat_id, attached.chat_id);
        assert_eq!(transcript_import, TranscriptImport::None);
        assert!(
            workspace
                .doc()
                .session_ref("user-a", &attached.chat_id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn ignores_omp_placeholder_title_before_session_metadata() {
        let temp = TempDir::new().unwrap();
        let path = fixture(
            temp.path(),
            "omp-placeholder.jsonl",
            &[
                serde_json::json!({"type":"title","v":1,"title":"","pad":"reserved"}),
                serde_json::json!({"type":"session","id":"omp-placeholder","cwd":"/repo","title":"Graceful staging restart","timestamp":"2026-08-05T12:00:00Z"}),
                serde_json::json!({"type":"message","id":"m1","message":{"role":"user","content":[{"type":"text","text":"Investigate staging"}]}}),
            ],
        );

        let session = candidate_from_omp(&path).unwrap();
        assert_eq!(session.candidate.title, "Graceful staging restart");
    }
    #[test]
    fn discover_omp_excludes_journals_without_a_user_turn() {
        let temp = TempDir::new().unwrap();
        fixture(
            temp.path(),
            "sessions/project/blank.jsonl",
            &[serde_json::json!({
                "type": "session",
                "id": "blank",
                "cwd": "/repo"
            })],
        );

        assert!(discover_omp(temp.path()).is_empty());
    }
    #[test]
    fn omp_watermark_lookup_uses_session_metadata_without_discovery_eligibility() {
        let temp = TempDir::new().unwrap();
        let path = fixture(
            temp.path(),
            "sessions/project/watermark.jsonl",
            &[serde_json::json!({
                "type": "session",
                "id": "watermark-session",
                "cwd": "/repo",
                "timestamp": "2026-08-20T04:11:10Z"
            })],
        );

        assert_eq!(
            omp_session_updated_at_with_root(temp.path(), "watermark-session", "/repo"),
            file_modified_ms(&path)
        );
    }

    #[test]
    fn omp_writer_state_controls_resumability_without_live_attachment() {
        let temp = TempDir::new().unwrap();
        let path = fixture(
            temp.path(),
            "omp-ownership.jsonl",
            &[serde_json::json!({
                "type": "session",
                "id": "omp-ownership",
                "cwd": "/repo"
            })],
        );
        for (state, busy_elsewhere, resumable, history_only) in [
            (
                comet_harness::omp::SessionWriterState::Active,
                Some(true),
                false,
                true,
            ),
            (
                comet_harness::omp::SessionWriterState::Inactive,
                Some(false),
                true,
                false,
            ),
            (
                comet_harness::omp::SessionWriterState::Unknown,
                None,
                false,
                true,
            ),
        ] {
            let candidate = candidate_from_omp_with_writer_state(&path, state)
                .unwrap()
                .candidate;
            assert!(!candidate.live_attachable);
            assert_eq!(candidate.busy_elsewhere, busy_elsewhere);
            assert_eq!(candidate.resumable, resumable);
            assert_eq!(candidate.history_only, history_only);
        }
    }

    #[test]
    fn materialized_omp_history_waits_for_its_writer_to_exit() {
        let temp = TempDir::new().unwrap();
        let path = fixture(
            temp.path(),
            "omp-transition.jsonl",
            &[serde_json::json!({
                "type": "session",
                "id": "omp-transition",
                "cwd": "/repo"
            })],
        );
        for writer_state in [
            comet_harness::omp::SessionWriterState::Active,
            comet_harness::omp::SessionWriterState::Unknown,
        ] {
            let candidate = candidate_from_omp_with_writer_state(&path, writer_state)
                .unwrap()
                .candidate;
            assert!(!materialized_session_needs_action(
                &candidate,
                candidate.updated_at.saturating_sub(1),
                true
            ));
        }

        let candidate = candidate_from_omp_with_writer_state(
            &path,
            comet_harness::omp::SessionWriterState::Inactive,
        )
        .unwrap()
        .candidate;
        assert!(materialized_session_needs_action(
            &candidate,
            candidate.updated_at.saturating_sub(1),
            true
        ));
        assert!(!materialized_session_needs_action(
            &candidate,
            candidate.updated_at,
            true
        ));
        assert!(materialized_session_needs_action(
            &candidate,
            candidate.updated_at,
            false
        ));
    }

    #[test]
    fn captures_omp_candidate_with_sessions_root_relative_path() {
        let temp = TempDir::new().unwrap();
        let roots = SessionRoots {
            claude: temp.path().join("claude"),
            codex: temp.path().join("codex"),
            omp: temp.path().join("omp"),
            prime: temp.path().join("prime"),
            prime_sessions: temp.path().join("pi/sessions"),
            opencode: temp.path().join("opencode.db"),
        };
        let path = fixture(
            &roots.omp,
            "sessions/by-cwd/omp-1.jsonl",
            &[
                serde_json::json!({"type":"session","id":"omp-1","cwd":"/repo","timestamp":"2026-08-05T12:00:00Z"}),
                serde_json::json!({"type":"message","id":"m1","message":{"role":"user","content":"hello"}}),
            ],
        );
        let candidate = candidate_from_omp(&path).unwrap();
        let expected_bytes = std::fs::read(&path).unwrap();
        let artifact = capture_omp_artifact_with_roots(&candidate.candidate.id, &roots).unwrap();
        assert_eq!(artifact.native_session_id, "omp-1");
        assert_eq!(artifact.cwd, "/repo");
        assert_eq!(artifact.storage_relative_path, "by-cwd/omp-1.jsonl");
        assert_eq!(artifact.byte_count, expected_bytes.len() as u64);
        assert_eq!(artifact.bytes, expected_bytes);
        assert_eq!(
            artifact.sha256,
            format!("{:x}", Sha256::digest(&artifact.bytes))
        );
        let by_native_session = capture_omp_artifact_for_session_with_roots(
            &candidate.candidate.session_id,
            &candidate.candidate.cwd,
            &roots,
        )
        .unwrap();
        assert_eq!(by_native_session, artifact);
        assert!(capture_omp_artifact_for_session_with_roots("omp-other", "/repo", &roots).is_err());
        assert!(capture_omp_artifact_for_session_with_roots("omp-1", "/other", &roots).is_err());
    }

    #[test]
    fn finds_one_omp_session_for_file_backed_capture_and_honors_cancellation() {
        let temp = TempDir::new().unwrap();
        let omp = temp.path().join("omp");
        fixture(
            &omp,
            "sessions/by-cwd/omp-1.jsonl",
            &[
                serde_json::json!({"type":"session","id":"omp-1","cwd":"/repo","timestamp":"2026-08-05T12:00:00Z"}),
                serde_json::json!({"type":"message","id":"m1","message":{"role":"user","content":"hello"}}),
            ],
        );
        let cancellation = tokio_util::sync::CancellationToken::new();
        let session = find_omp_session_for_capture(&omp, "omp-1", "/repo", &cancellation).unwrap();
        assert_eq!(session.candidate.session_id, "omp-1");
        cancellation.cancel();
        assert!(find_omp_session_for_capture(&omp, "omp-1", "/repo", &cancellation).is_err());
    }

    #[test]
    fn detects_imported_worktree_branch_and_checkout_identity() {
        fn git(cwd: &Path, args: &[&str]) {
            let status = ProcessCommand::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }

        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        let worktree = temp.path().join("imported-worktree");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        fs::write(root.join("README"), "fixture").unwrap();
        git(&root, &["add", "README"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Comet Test",
                "-c",
                "user.email=comet@example.test",
                "commit",
                "-m",
                "fixture",
            ],
        );
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "comet/imported-worktree",
                worktree.to_str().unwrap(),
            ],
        );

        let metadata = session_repo_metadata(&worktree, "device-1").unwrap();
        let root_metadata = session_repo_metadata(&root, "device-1").unwrap();
        assert_eq!(metadata.branch, "comet/imported-worktree");
        assert_eq!(
            metadata.space_path,
            fs::canonicalize(&root).unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(metadata.space_checkout_id, root_metadata.checkout_id);
        assert_eq!(metadata.space_checkout_id, root_metadata.space_checkout_id);
        assert_ne!(metadata.checkout_id, root_metadata.checkout_id);
    }

    #[tokio::test]
    async fn listing_is_read_only_and_attach_reconciles_imported_worktree() {
        fn git(cwd: &Path, args: &[&str]) {
            let status = ProcessCommand::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }

        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        let worktree = temp.path().join("imported-worktree");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        fs::write(root.join("README"), "fixture").unwrap();
        git(&root, &["add", "README"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Comet Test",
                "-c",
                "user.email=comet@example.test",
                "commit",
                "-m",
                "fixture",
            ],
        );
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "comet/imported-worktree",
                worktree.to_str().unwrap(),
            ],
        );
        let session_path = fixture(
            temp.path(),
            "omp/sessions/by-cwd/omp-1.jsonl",
            &[
                serde_json::json!({
                    "type": "session",
                    "id": "omp-1",
                    "cwd": worktree,
                    "timestamp": "2026-08-05T12:00:00Z"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "m1",
                    "message": {"role": "user", "content": "Do not import me while listing"}
                }),
            ],
        );
        let session = candidate_from_omp(&session_path).unwrap();
        let store = Arc::new(DocsStore::open(temp.path().join("docs")).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            WorkspaceHostConfig {
                device_id: "device-a".into(),
                device_name: "Test Mac".into(),
                platform: "test".into(),
                project_scope: "project-a".into(),
                user_id: "user-a".into(),
                edge: None,
            },
        )
        .unwrap();
        let old_path = fs::canonicalize(&worktree)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let old_space_id = local_space_id(&old_path);
        workspace
            .create_space(&old_space_id, "device-a", &old_path, None, true)
            .unwrap();
        workspace
            .create_chat(
                &session.candidate.chat_id,
                &old_space_id,
                None,
                Some(old_path.clone()),
            )
            .unwrap();
        workspace
            .set_chat_activity(
                &session.candidate.chat_id,
                Some(session.candidate.updated_at),
                Some(session.candidate.created_at),
            )
            .unwrap();
        let doc_host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: "device-a".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        doc_host.set_workspace(workspace.clone());

        let metadata = session_repo_metadata(&worktree, "device-a").unwrap();
        let canonical_space_id = local_space_id(&metadata.space_path);
        let listed = list_discovered(vec![session.clone()], &workspace).unwrap();
        assert_eq!(
            listed.len(),
            1,
            "an unowned resumable OMP session must remain attachable"
        );
        let chat = workspace
            .doc()
            .chat(&session.candidate.chat_id)
            .unwrap()
            .unwrap();
        assert_eq!(chat.space_id.as_deref(), Some(old_space_id.as_str()));
        assert_eq!(chat.branch, None);
        assert!(
            workspace
                .read_spaces()
                .unwrap()
                .iter()
                .all(|space| space.id != canonical_space_id),
            "listing must not reconcile workspace rows"
        );
        assert!(
            store
                .load_snapshot(&session.candidate.chat_id)
                .unwrap()
                .is_none()
        );

        let (_, transcript_import) =
            materialize_discovered(&session, &workspace, &doc_host).unwrap();
        assert_eq!(transcript_import, TranscriptImport::Full);
        apply_transcript_import(transcript_import, &session, &workspace, &doc_host).unwrap();
        assert!(
            list_discovered(vec![session.clone()], &workspace)
                .unwrap()
                .is_empty(),
            "an up-to-date imported OMP chat must not be listed twice"
        );
        let chat = workspace
            .doc()
            .chat(&session.candidate.chat_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            chat.harness_session_id.as_deref(),
            Some(session.candidate.session_id.as_str())
        );
        let session_doc = doc_host.open(&session.candidate.chat_id).unwrap().doc_arc();
        let imported_count = session_doc.read_entries().unwrap().len();
        let mut advanced_owned_session = session.clone();
        advanced_owned_session.candidate.updated_at += 1;
        assert_eq!(
            list_discovered(vec![advanced_owned_session.clone()], &workspace)
                .unwrap()
                .len(),
            1,
            "an advanced OMP journal must expose an incremental refresh"
        );
        let (_, transcript_import) =
            materialize_discovered(&advanced_owned_session, &workspace, &doc_host).unwrap();
        assert_eq!(
            transcript_import,
            TranscriptImport::After(session.candidate.updated_at)
        );
        apply_transcript_import(
            transcript_import,
            &advanced_owned_session,
            &workspace,
            &doc_host,
        )
        .unwrap();
        assert_eq!(
            session_doc.read_entries().unwrap().len(),
            imported_count,
            "journal mtime drift must not duplicate Comet-owned messages"
        );
        assert!(
            list_discovered(vec![advanced_owned_session.clone()], &workspace)
                .unwrap()
                .is_empty(),
            "a no-op refresh must advance the imported journal watermark"
        );

        let comet_prompt_at = advanced_owned_session.candidate.updated_at + 1;
        let comet_reply_at = comet_prompt_at + 1;
        for entry in [
            SessionMessageEntry {
                id: "comet-owned-prompt".into(),
                role: MessageRole::User,
                parts: vec![MessagePart::Text {
                    id: "comet-owned-prompt-text".into(),
                    text: "continue".into(),
                }],
                created_at: comet_prompt_at,
                device_id: "device-a".into(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
            },
            SessionMessageEntry {
                id: "comet-owned-reply".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    id: "comet-owned-reply-text".into(),
                    text: "Comet reply".into(),
                }],
                created_at: comet_reply_at,
                device_id: "device-a".into(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
            },
        ] {
            session_doc.push_message(&entry).unwrap();
        }
        workspace
            .set_chat_activity(
                &session.candidate.chat_id,
                Some(comet_reply_at),
                Some(session.candidate.created_at),
            )
            .unwrap();
        let native_echo_at = comet_reply_at + 1;
        let native_reply_at = native_echo_at + 1;
        {
            use std::io::Write as _;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&session_path)
                .unwrap();
            for record in [
                serde_json::json!({
                    "type": "message",
                    "id": "native-echo-after-comet",
                    "timestamp": DateTime::<Utc>::from_timestamp_millis(native_echo_at)
                        .unwrap()
                        .to_rfc3339(),
                    "message": {"role": "user", "content": "continue"}
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "native-reply-after-comet",
                    "timestamp": DateTime::<Utc>::from_timestamp_millis(native_reply_at)
                        .unwrap()
                        .to_rfc3339(),
                    "message": {"role": "assistant", "content": "Comet reply"}
                }),
            ] {
                writeln!(file, "{record}").unwrap();
            }
        }
        let completed_comet_turn = candidate_from_omp_with_writer_state(
            &session_path,
            comet_harness::omp::SessionWriterState::Inactive,
        )
        .unwrap();
        let comet_turn_watermark = completed_comet_turn.candidate.updated_at;
        workspace
            .set_chat_activity(&session.candidate.chat_id, Some(comet_turn_watermark), None)
            .unwrap();
        assert!(
            list_discovered(vec![completed_comet_turn], &workspace)
                .unwrap()
                .is_empty(),
            "the Done watermark must cover native records already rendered by Comet"
        );

        let external_prompt_at = comet_turn_watermark + 1;
        let external_reply_at = external_prompt_at + 1;
        {
            use std::io::Write as _;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&session_path)
                .unwrap();
            for record in [
                serde_json::json!({
                    "type": "message",
                    "id": "external-repeated-prompt",
                    "timestamp": DateTime::<Utc>::from_timestamp_millis(external_prompt_at)
                        .unwrap()
                        .to_rfc3339(),
                    "message": {"role": "user", "content": "continue"}
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "external-reply",
                    "timestamp": DateTime::<Utc>::from_timestamp_millis(external_reply_at)
                        .unwrap()
                        .to_rfc3339(),
                    "message": {"role": "assistant", "content": "External reply"}
                }),
            ] {
                writeln!(file, "{record}").unwrap();
            }
        }
        let refreshed = candidate_from_omp_with_writer_state(
            &session_path,
            comet_harness::omp::SessionWriterState::Inactive,
        )
        .unwrap();
        assert_eq!(
            list_discovered(vec![refreshed.clone()], &workspace)
                .unwrap()
                .len(),
            1
        );
        let (_, transcript_import) =
            materialize_discovered(&refreshed, &workspace, &doc_host).unwrap();
        assert_eq!(
            transcript_import,
            TranscriptImport::After(comet_turn_watermark)
        );
        apply_transcript_import(transcript_import, &refreshed, &workspace, &doc_host).unwrap();
        let refreshed_entries = session_doc.read_entries().unwrap();
        assert_eq!(refreshed_entries.len(), imported_count + 4);
        assert_eq!(
            refreshed_entries
                .iter()
                .filter(|entry| entry.parts.iter().find_map(text_part) == Some("continue"))
                .count(),
            2,
            "a repeated external prompt after the exact Done watermark must be preserved"
        );
        assert_eq!(
            refreshed_entries
                .iter()
                .filter(|entry| { entry.parts.iter().find_map(text_part) == Some("Comet reply") })
                .count(),
            1,
            "the native copy of Comet's live reply must remain behind the watermark"
        );
        assert_eq!(
            refreshed_entries
                .iter()
                .filter(|entry| {
                    entry.parts.iter().find_map(text_part) == Some("External reply")
                })
                .count(),
            1
        );
        assert!(
            list_discovered(vec![refreshed], &workspace)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            chat.cwd.as_deref(),
            Some(session.candidate.cwd.as_str()),
            "chat keeps the native session's exact cwd while its space is canonical"
        );
        assert_eq!(chat.space_id.as_deref(), Some(canonical_space_id.as_str()));
        assert_eq!(chat.branch.as_deref(), Some("comet/imported-worktree"));
        assert_eq!(
            chat.checkout_id.as_deref(),
            Some(metadata.checkout_id.as_str())
        );
        assert!(
            workspace
                .read_spaces()
                .unwrap()
                .iter()
                .all(|space| space.id != old_space_id)
        );
        let mut fresh_session = session.clone();
        fresh_session.candidate.id = "omp:fresh".into();
        fresh_session.candidate.chat_id = "local-chat-fresh".into();
        fresh_session.candidate.session_id = "omp-fresh".into();
        let fresh = list_discovered(vec![fresh_session], &workspace).unwrap();
        assert_eq!(fresh.len(), 1);
        assert!(workspace.doc().chat("local-chat-fresh").unwrap().is_none());
        assert!(store.load_snapshot("local-chat-fresh").unwrap().is_none());

        workspace
            .create_chat(
                "owned-chat",
                &canonical_space_id,
                None,
                Some(old_path.clone()),
            )
            .unwrap();
        workspace.set_chat_harness_session("owned-chat", &session.candidate.session_id, &old_path);
        let mut owned_session = session;
        owned_session.candidate.chat_id = "local-chat-unmaterialized".into();
        assert!(
            list_discovered(vec![owned_session], &workspace)
                .unwrap()
                .is_empty(),
            "Comet-owned native sessions must not be listed"
        );
    }

    #[test]
    fn omp_parent_sessions_survive_newer_nested_transcript_saturation() {
        let temp = TempDir::new().unwrap();
        let parent_paths = ["parent-one", "parent-two"].map(|session_id| {
            fixture(
                temp.path(),
                &format!("sessions/project/{session_id}.jsonl"),
                &[
                    serde_json::json!({
                        "type": "session",
                        "id": session_id,
                        "cwd": "/repo",
                        "timestamp": "2026-08-01T00:00:00Z"
                    }),
                    serde_json::json!({
                        "type": "message",
                        "id": format!("{session_id}-user"),
                        "message": {"role": "user", "content": "Parent session"}
                    }),
                ],
            )
        });
        let old = UNIX_EPOCH + std::time::Duration::from_secs(1);
        for path in &parent_paths {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        for index in 0..=MAX_CANDIDATES_PER_HARNESS {
            fixture(
                temp.path(),
                &format!("sessions/project/parent-one/__advisor.{index}.jsonl"),
                &[serde_json::json!({
                    "type": "session",
                    "id": format!("nested-{index}"),
                    "cwd": "/repo"
                })],
            );
        }

        let saturated =
            recent_jsonl_files(&temp.path().join("sessions"), MAX_CANDIDATES_PER_HARNESS);
        assert!(
            parent_paths
                .iter()
                .all(|parent| !saturated.contains(parent))
        );
        let sessions = discover_omp(temp.path());
        assert_eq!(sessions.len(), 2);
        assert!(["parent-one", "parent-two"].into_iter().all(|session_id| {
            sessions
                .iter()
                .any(|session| session.candidate.session_id == session_id)
        }));
    }

    #[test]
    fn discovers_prime_session_in_current_pi_store() {
        let temp = TempDir::new().unwrap();
        let legacy_root = temp.path().join("prime");
        let current_sessions = temp.path().join("pi/agent/sessions");
        let path = fixture(
            &current_sessions,
            "pi-current.jsonl",
            &[serde_json::json!({
                "type": "session",
                "id": "pi-current",
                "cwd": "/repo",
                "rlmDepth": 0,
                "timestamp": "2026-08-06T12:00:00Z"
            })],
        );

        let sessions = discover_prime_agent(&legacy_root, &current_sessions);
        assert_eq!(sessions.len(), 1);
        let candidate = &sessions[0].candidate;
        assert_eq!(candidate.session_id, "pi-current");
        assert!(candidate.resumable);
        assert!(!candidate.history_only);
        assert_eq!(candidate.busy_elsewhere, None);
        assert!(
            candidate_from_omp_with_writer_state(
                &path,
                comet_harness::omp::SessionWriterState::Inactive
            )
            .is_none()
        );
    }

    #[test]
    fn deduplicates_prime_native_ids_across_session_stores() {
        let temp = TempDir::new().unwrap();
        let legacy_root = temp.path().join("prime");
        let current_sessions = temp.path().join("pi/agent/sessions");
        for (root, relative) in [
            (&legacy_root, "sessions/duplicate.jsonl"),
            (&current_sessions, "duplicate.jsonl"),
        ] {
            fixture(
                root,
                relative,
                &[serde_json::json!({
                    "type": "session",
                    "id": "same-native-id",
                    "cwd": "/repo",
                    "rlmDepth": 0,
                    "timestamp": "2026-08-06T12:00:00Z"
                })],
            );
        }

        let sessions = discover_prime_agent(&legacy_root, &current_sessions);
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.candidate.session_id == "same-native-id")
                .count(),
            1
        );
    }

    #[test]
    fn live_prime_metadata_is_allowlisted_deduplicated_and_fails_closed_without_transcript() {
        let temp = TempDir::new().unwrap();
        let legacy_root = temp.path().join("prime");
        let current_sessions = temp.path().join("pi/agent/sessions");
        let durable_path = fixture(
            &current_sessions,
            "live-file.jsonl",
            &[serde_json::json!({
                "type": "session",
                "id": "live-file",
                "cwd": "/repo",
                "rlmDepth": 0,
                "timestamp": "2026-08-06T13:00:00Z"
            })],
        );
        let outside_path = fixture(
            temp.path(),
            "outside.jsonl",
            &[serde_json::json!({
                "type": "session",
                "id": "outside",
                "cwd": "/outside",
                "rlmDepth": 0
            })],
        );
        let metadata_root = legacy_root.join("daemon-workers/supervisor");
        fs::create_dir_all(&metadata_root).unwrap();
        fs::write(
            metadata_root.join("durable.json"),
            serde_json::json!({
                "rootSessionId": "live-file",
                "sessionFile": durable_path,
                "createdAt": "2026-08-06T13:00:00Z",
                "updatedAt": "2026-08-06T14:00:00Z",
                "lifecycle": "ready",
                "authenticationToken": "must-never-surface",
                "createCommand": {"config": {"cwd": "/repo"}}
            })
            .to_string(),
        )
        .unwrap();
        let pathless_metadata = metadata_root.join("pathless.json");
        fs::write(
            &pathless_metadata,
            serde_json::json!({
                "rootSessionId": "live-without-transcript",
                "sessionFile": outside_path,
                "createdAt": "2026-08-06T14:00:00Z",
                "updatedAt": "2026-08-06T15:00:00Z",
                "lifecycle": "ready",
                "authenticationToken": "also-must-never-surface",
                "createCommand": {"config": {"cwd": "/repo"}}
            })
            .to_string(),
        )
        .unwrap();

        let sessions = discover_prime_agent(&legacy_root, &current_sessions);
        assert_eq!(sessions.len(), 2);
        assert!(
            sessions
                .windows(2)
                .all(|pair| pair[0].candidate.updated_at >= pair[1].candidate.updated_at)
        );
        let durable = sessions
            .iter()
            .find(|session| session.candidate.session_id == "live-file")
            .unwrap();
        assert!(durable.candidate.resumable);
        assert!(!durable.candidate.history_only);
        assert_eq!(durable.candidate.busy_elsewhere, Some(true));
        assert_eq!(
            fs::canonicalize(&durable.path).unwrap(),
            fs::canonicalize(&durable_path).unwrap()
        );
        let unavailable = sessions
            .iter()
            .find(|session| session.candidate.session_id == "live-without-transcript")
            .unwrap();
        assert!(!unavailable.candidate.live_attachable);
        assert!(!unavailable.candidate.resumable);
        assert!(!unavailable.candidate.history_only);
        assert_eq!(unavailable.candidate.busy_elsewhere, Some(true));
        assert_eq!(unavailable.path, pathless_metadata);
        let serialized = serde_json::to_string(
            &sessions
                .iter()
                .map(|session| &session.candidate)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(!serialized.contains("must-never-surface"));
    }

    #[test]
    fn discovery_bound_caps_large_native_stores() {
        let temp = TempDir::new().unwrap();
        for index in 0..=MAX_CANDIDATES_PER_HARNESS {
            fixture(
                temp.path(),
                &format!("sessions/session-{index}.jsonl"),
                &[serde_json::json!({"type":"session","id":index,"cwd":"/repo"})],
            );
        }
        assert_eq!(
            recent_jsonl_files(temp.path(), MAX_CANDIDATES_PER_HARNESS).len(),
            MAX_CANDIDATES_PER_HARNESS
        );
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

    #[test]
    fn omp_import_follows_only_the_active_parent_linked_branch() {
        let temp = TempDir::new().unwrap();
        let path = fixture(
            temp.path(),
            "branched-session.jsonl",
            &[
                serde_json::json!({"type":"session","id":"omp-1","cwd":"/repo","timestamp":"2026-08-05T12:00:00Z"}),
                serde_json::json!({"type":"message","id":"u1","parentId":null,"timestamp":"2026-08-05T12:01:00Z","message":{"role":"user","content":"Question"}}),
                serde_json::json!({"type":"message","id":"a1","parentId":"u1","timestamp":"2026-08-05T12:02:00Z","message":{"role":"assistant","content":"First answer"}}),
                serde_json::json!({"type":"message","id":"abandoned-u","parentId":"a1","timestamp":"2026-08-05T12:03:00Z","message":{"role":"user","content":"Abandoned branch"}}),
                serde_json::json!({"type":"message","id":"abandoned-a","parentId":"abandoned-u","timestamp":"2026-08-05T12:04:00Z","message":{"role":"assistant","content":"Wrong context"}}),
                serde_json::json!({"type":"message","id":"active-u","parentId":"a1","timestamp":"2026-08-05T12:05:00Z","message":{"role":"user","content":"Active branch"}}),
                serde_json::json!({"type":"message","id":"active-a","parentId":"active-u","timestamp":"2026-08-05T12:06:00Z","message":{"role":"assistant","content":"Current answer"}}),
                serde_json::json!({"type":"session_exit","id":"exit","parentId":"active-a","timestamp":"2026-08-05T12:07:00Z"}),
            ],
        );
        let session = candidate_from_omp(&path).unwrap();
        let entries = load_transcript(&session, "device-1").unwrap();
        let text = entries
            .iter()
            .flat_map(|entry| entry.parts.iter().filter_map(text_part))
            .collect::<Vec<_>>();

        assert_eq!(
            text,
            [
                "Question",
                "First answer",
                "Active branch",
                "Current answer"
            ]
        );
    }
}
