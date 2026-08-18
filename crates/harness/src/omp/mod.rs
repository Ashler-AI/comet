//! OMP harness adapter over OMP's native RPC mode (`omp --mode rpc`).
//!
//! Comet owns execution through JSONL frames on the child's stdio; OMP's
//! read-only `models --json` command supplies the selectable provider/model
//! catalog. One persistent child serves every turn, and mid-turn followups
//! steer the live turn between tool calls (step-boundary semantics) instead
//! of queueing behind it — the reason this adapter left ACP, whose
//! `session/prompt` is strictly turn-serial. The ACP client in [`rpc`] and
//! the [`run_acp`] loop remain solely for Prime Agent.

pub(crate) mod rpc;
pub(crate) mod rpc_mode;

use std::collections::VecDeque;
use std::ffi::OsString;
use std::future::Future;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt as _;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::Instant;

use comet_proto::{
    AgentActivity, AgentActivityStatus, AgentEvent, DoneStatus, HarnessCommand, HarnessId, Model,
    OmpAdvisorConfig, OmpAdvisorSyncBacklog, ReasoningLevel, RunRequest, SteeringMode, TodoItem,
    ToolCall, UserInputQuestion,
};

use crate::{Harness, HarnessError, InferenceRoute, RunControls};
use rpc::{Incoming, RpcClient};

const ACP_PROTOCOL_VERSION: i64 = 1;
/// Oldest OMP this engine can drive — the floor behind the "Update required"
/// state in Settings → Agents. Tracks the version this release was verified
/// against (also the installer's pinned bootstrap).
pub const MIN_OMP_VERSION: &str = "17.2.9";
const ACP_INITIALIZE_DEADLINE: Duration = Duration::from_secs(15);
const ACP_SESSION_DEADLINE: Duration = Duration::from_secs(30);
const ACP_CONFIGURE_DEADLINE: Duration = Duration::from_secs(15);
const ACP_PROMPT_INACTIVITY_LOG_INTERVAL: Duration = Duration::from_secs(300);
// Cold project capability discovery can legitimately exceed the old 15-second
// boundary in large repositories. Warm launches still return as soon as ready.
const RPC_STARTUP_DEADLINE: Duration = Duration::from_secs(60);
// Includes the startup boundary plus the command stream's two-second settle.
const RPC_COMMAND_CATALOG_DEADLINE: Duration = Duration::from_secs(65);
const SCAFFOLD_PROFILE: &str = "scaffold-host";
const AUTH_BROKER_URL_ENV: &str = "OMP_AUTH_BROKER_URL";
const AUTH_BROKER_TOKEN_ENV: &str = "OMP_AUTH_BROKER_TOKEN";
const AUTH_BROKER_TOKEN_FILE_ENV: &str = "OMP_AUTH_BROKER_TOKEN_FILE";
const PI_CONFIG_FILES_ENV: &str = "PI_CONFIG_FILES";
const LOCAL_RUNTIME_ENV: &str = "COMET_LOCAL_AGENT_RUNTIME";
const SCAFFOLD_INFERENCE_PROFILE_FILE: &str = "omp-inference/profile.json";
const SCAFFOLD_INFERENCE_PROFILE_BYTES: u64 = 4 * 1024;
// Agent Auth accepts at most 8 MiB per inference request. OMP's 3840x2400
// Computer default can retain multi-megabyte PNGs in Responses history, so use
// its documented coordinate-safe capture cap for every Comet-owned OMP run.
const OMP_RUN_CONFIG: &[u8] = br#"retry:
  enabled: true
  maxRetries: 1
  baseDelayMs: 1000
  provider:
    maxRetries: 0
computer:
  maxWidth: 1280
  maxHeight: 896
"#;
pub const OMP_SUPERVISOR_MARKER: &str = "__comet-omp-supervisor";

/// Run the hidden OMP supervisor command when this process was invoked for it.
///
/// The supervisor is a separate process in the OMP process group. It inherits
/// the engine's RPC stdio unchanged, watches the exact engine parent PID, and
/// terminates OMP plus its tool descendants when that parent disappears.
pub fn run_supervisor_from_env() -> Option<Result<(), HarnessError>> {
    let mut args = std::env::args_os();
    let _program = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(OMP_SUPERVISOR_MARKER)) {
        return None;
    }
    Some(run_omp_supervisor(args))
}

#[cfg(unix)]
fn run_omp_supervisor(mut args: impl Iterator<Item = OsString>) -> Result<(), HarnessError> {
    use std::os::unix::process::ExitStatusExt as _;

    let parent_pid = args
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse::<u32>().ok()))
        .ok_or_else(|| HarnessError::Protocol("OMP supervisor has no valid parent PID".into()))?;
    let executable = args
        .next()
        .ok_or_else(|| HarnessError::Protocol("OMP supervisor has no child executable".into()))?;
    let mut child = ProcessCommand::new(executable)
        .args(args)
        .spawn()
        .map_err(HarnessError::Io)?;

    // Installed after spawn so OMP keeps the default disposition. The
    // supervisor must survive the group's graceful SIGTERM long enough to reap
    // OMP and escalate if OMP ignores it.
    // SAFETY: installing SIG_IGN for SIGTERM has no pointer or lifetime input.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    loop {
        if let Some(status) = child.try_wait()? {
            std::process::exit(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
            );
        }
        // A Unix child is never attached to a reused PID. Once reparented, an
        // equal numeric PID cannot make this relationship live again.
        // SAFETY: getppid/getpgrp have no preconditions.
        let parent_changed = unsafe { libc::getppid() } != parent_pid as libc::pid_t;
        if parent_changed {
            // SAFETY: the engine launched this supervisor as the process-group
            // leader; a negative id targets only that dedicated group.
            let group = unsafe { libc::getpgrp() };
            unsafe {
                libc::kill(-group, libc::SIGTERM);
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if child.try_wait()?.is_some() {
                    std::process::exit(0);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            // SAFETY: same dedicated group; SIGKILL intentionally includes the
            // supervisor itself so no guardian can become the next orphan.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
            std::process::abort();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(not(unix))]
fn run_omp_supervisor(_args: impl Iterator<Item = OsString>) -> Result<(), HarnessError> {
    Err(HarnessError::Protocol(
        "OMP supervision requires a Unix host".into(),
    ))
}

#[derive(Debug)]
pub(crate) struct OmpRunConfig {
    path: PathBuf,
}

impl OmpRunConfig {
    fn create() -> Result<Self, HarnessError> {
        let temp_dir = std::env::temp_dir();
        let temp_dir = if temp_dir.is_absolute() {
            temp_dir
        } else {
            std::env::current_dir()?.join(temp_dir)
        };
        let path = temp_dir.join(format!("comet-omp-{}.yml", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        if let Err(error) = file.write_all(OMP_RUN_CONFIG) {
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        Ok(Self { path })
    }

    fn apply(&self, command: &mut Command) -> Result<(), HarnessError> {
        let value =
            config_overlay_paths(std::env::var_os(PI_CONFIG_FILES_ENV).as_deref(), &self.path)?;
        command.env(PI_CONFIG_FILES_ENV, value);
        Ok(())
    }
}

fn config_overlay_paths(
    inherited: Option<&std::ffi::OsStr>,
    run_config: &Path,
) -> Result<OsString, HarnessError> {
    let mut paths = inherited
        .map(|value| std::env::split_paths(value).collect::<Vec<_>>())
        .unwrap_or_default();
    paths.push(run_config.to_path_buf());
    std::env::join_paths(paths).map_err(|error| {
        HarnessError::Protocol(format!(
            "Could not construct OMP config overlay path: {error}"
        ))
    })
}

impl Drop for OmpRunConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScaffoldInferenceProfile {
    profile: String,
    model: String,
}

fn scaffold_inference_model_at(
    runtime_dir: &Path,
    requested_model: Option<&str>,
) -> Result<String, HarnessError> {
    let path = runtime_dir.join(SCAFFOLD_INFERENCE_PROFILE_FILE);
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() == 0
        || metadata.len() > SCAFFOLD_INFERENCE_PROFILE_BYTES
    {
        return Err(HarnessError::Protocol(
            "Scaffold OMP inference profile is not a bounded regular file".into(),
        ));
    }
    let profile: ScaffoldInferenceProfile =
        serde_json::from_slice(&std::fs::read(path)?).map_err(|error| {
            HarnessError::Protocol(format!("invalid Scaffold OMP inference profile: {error}"))
        })?;
    if profile.profile != SCAFFOLD_PROFILE
        || profile.model.len() > 256
        || profile.model.chars().any(char::is_whitespace)
    {
        return Err(HarnessError::Protocol(
            "Scaffold OMP inference profile has an invalid model binding".into(),
        ));
    }
    let (requested_provider, requested_model) = requested_model
        .and_then(|model| model.rsplit_once('/'))
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .ok_or_else(|| {
            HarnessError::Protocol(
                "Scaffold OMP runs require a provider-qualified model selection".into(),
            )
        })?;
    let routed_provider = match requested_provider {
        "openai-codex" => "scaffold-openai",
        "anthropic" => "scaffold-anthropic",
        _ => {
            return Err(HarnessError::Protocol(
                "Scaffold OMP inference profile does not match the requested model".into(),
            ));
        }
    };
    let (profile_provider, routed_model) = profile.model.rsplit_once('/').ok_or_else(|| {
        HarnessError::Protocol("Scaffold OMP inference profile has an invalid model binding".into())
    })?;
    if profile_provider != routed_provider || requested_model != routed_model {
        return Err(HarnessError::Protocol(
            "Scaffold OMP inference profile does not match the requested model".into(),
        ));
    }
    Ok(profile.model)
}

fn configure_scaffold_inference_profile(
    command: &mut Command,
    requested_model: Option<&str>,
) -> Result<(), HarnessError> {
    let runtime_dir = std::env::var_os("SCAFFOLD_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| {
            HarnessError::Protocol("SCAFFOLD_RUNTIME_DIR is required for Scaffold OMP".into())
        })?;
    let model = scaffold_inference_model_at(&runtime_dir, requested_model)?;
    command.args(["--model", &model]);
    Ok(())
}

const OMP_SESSION_DIRECTORY_SCAN_LIMIT: usize = 10_000;
const OMP_SESSION_HEADER_BYTES: u64 = 64 * 1024;

fn acp_assistant_message_id(session_id: &str, run_nonce: &uuid::Uuid, turn_number: u64) -> String {
    format!("acp-{session_id}-{run_nonce}-{turn_number}")
}

/// OMP's `--thinking` flag ladder (off|minimal|low|medium|high|xhigh|max).
/// Levels above OMP's ladder clamp to max.
fn thinking_flag(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max
        | ReasoningLevel::Ultra
        | ReasoningLevel::Ultracode
        | ReasoningLevel::Ultrathink => "max",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisorConfigUpdate {
    Enabled(bool),
    Model(String),
    Subagents(bool),
    SyncBacklog(OmpAdvisorSyncBacklog),
    ImmuneTurns(u32),
}
/// Whether another process currently owns an OMP session journal for writing.
///
/// OMP 17.2.9 has no persisted local attach endpoint or writer lock. Its
/// session manager keeps an append descriptor open between writes for an active
/// persisted session. We probe the exact path and count only write-capable file
/// descriptors; read-only tails, editors, and log viewers do not own the
/// session. A failed or incomplete probe remains fail-closed as `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionWriterState {
    Active,
    Inactive,
    Unknown,
}

fn parse_lsof_writer_pids(stdout: &[u8]) -> Option<Vec<u32>> {
    let mut pid = None;
    let mut writers = Vec::new();
    let mut file_open = false;
    let mut access_seen = false;

    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        match line.first().copied()? {
            b'p' => {
                if file_open && !access_seen {
                    return None;
                }
                let value = std::str::from_utf8(&line[1..]).ok()?.parse::<u32>().ok()?;
                pid = Some(value);
                file_open = false;
                access_seen = false;
            }
            b'f' => {
                if file_open && !access_seen {
                    return None;
                }
                pid?;
                file_open = true;
                access_seen = false;
            }
            b'a' => {
                if !file_open || access_seen {
                    return None;
                }
                access_seen = true;
                match line.get(1).copied()? {
                    b'w' | b'u' => {
                        let pid = pid?;
                        if !writers.contains(&pid) {
                            writers.push(pid);
                        }
                    }
                    b'r' => {}
                    _ => return None,
                }
            }
            // `n` is requested for auditability but ownership is established by
            // the exact path argument plus the access field above.
            b'n' => {}
            _ => return None,
        }
    }
    if file_open && !access_seen {
        return None;
    }
    Some(writers)
}

fn session_writer_pids_with(path: &Path, executable: &Path) -> Option<Vec<u32>> {
    let output = ProcessCommand::new(executable)
        .args(["-F", "pfan", "--"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        return parse_lsof_writer_pids(&output.stdout);
    }
    if output.status.code() == Some(1) && output.stderr.is_empty() {
        return Some(Vec::new());
    }
    None
}

fn session_writer_state_with(path: &Path, executable: &Path) -> Option<SessionWriterState> {
    session_writer_pids_with(path, executable).map(|pids| {
        if pids.is_empty() {
            SessionWriterState::Inactive
        } else {
            SessionWriterState::Active
        }
    })
}

#[cfg(any(target_os = "linux", test))]
fn fdinfo_text_is_write_capable(text: &str) -> Option<bool> {
    let flags = text.lines().find_map(|line| {
        line.strip_prefix("flags:")
            .map(str::trim)
            .and_then(|value| u32::from_str_radix(value, 8).ok())
    })?;
    let access = flags as i32 & libc::O_ACCMODE;
    Some(access == libc::O_WRONLY || access == libc::O_RDWR)
}

#[cfg(target_os = "linux")]
fn fdinfo_is_write_capable(path: &Path) -> Option<bool> {
    fdinfo_text_is_write_capable(&std::fs::read_to_string(path).ok()?)
}

#[cfg(target_os = "linux")]
fn linux_session_writer_pids(path: &Path) -> Option<Vec<u32>> {
    use std::os::unix::fs::MetadataExt as _;

    let target = std::fs::metadata(path).ok()?;
    let processes = std::fs::read_dir("/proc").ok()?;
    // SAFETY: geteuid has no preconditions and does not retain pointers.
    let effective_uid = unsafe { libc::geteuid() };
    let mut incomplete = false;
    let mut writers = Vec::new();
    for process in processes {
        let Ok(process) = process else {
            incomplete = true;
            continue;
        };
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(process_metadata) = process.metadata() else {
            continue;
        };
        if process_metadata.uid() != effective_uid {
            continue;
        }
        let descriptors = match std::fs::read_dir(process.path().join("fd")) {
            Ok(descriptors) => descriptors,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        for descriptor in descriptors {
            let Ok(descriptor) = descriptor else {
                incomplete = true;
                continue;
            };
            let descriptor_path = descriptor.path();
            match std::fs::metadata(&descriptor_path) {
                Ok(metadata)
                    if metadata.dev() == target.dev() && metadata.ino() == target.ino() =>
                {
                    let fdinfo = process.path().join("fdinfo").join(descriptor.file_name());
                    match fdinfo_is_write_capable(&fdinfo) {
                        Some(true) => {
                            writers.push(pid);
                            break;
                        }
                        Some(false) => {}
                        None if !descriptor_path.exists() => {}
                        None => incomplete = true,
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => incomplete = true,
            }
        }
    }
    (!incomplete).then_some(writers)
}

fn session_writer_pids(path: &Path) -> Option<Vec<u32>> {
    #[cfg(target_os = "linux")]
    if let Some(pids) = linux_session_writer_pids(path) {
        return Some(pids);
    }

    for executable in [
        Path::new("/usr/sbin/lsof"),
        Path::new("/usr/bin/lsof"),
        Path::new("lsof"),
    ] {
        if let Some(pids) = session_writer_pids_with(path, executable) {
            return Some(pids);
        }
    }
    None
}

/// Probe one exact OMP JSONL path for a live writer.
///
/// Linux uses `/proc/<pid>/fdinfo`, so Scaffold images do not need an `lsof`
/// package. macOS and other Unix hosts resolve the native absolute `lsof`
/// locations before consulting `PATH`. A missing tool or failed/incomplete
/// probe is `Unknown`, never `Inactive`.
pub fn session_writer_state(path: &Path) -> SessionWriterState {
    match session_writer_pids(path) {
        Some(pids) if pids.is_empty() => SessionWriterState::Inactive,
        Some(_) => SessionWriterState::Active,
        None => SessionWriterState::Unknown,
    }
}
#[cfg(unix)]
#[derive(Debug, Clone)]
struct ProcessIdentity {
    pid: u32,
    uid: u32,
    ppid: u32,
    pgid: i32,
    executable: PathBuf,
    command: String,
}

#[cfg(any(target_os = "linux", test))]
fn linux_parent_and_group(stat: &str) -> Option<(u32, i32)> {
    let rest = stat.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse::<u32>().ok()?;
    let pgid = fields.next()?.parse::<i32>().ok()?;
    Some((ppid, pgid))
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let root = PathBuf::from(format!("/proc/{pid}"));
    let uid = std::fs::metadata(&root).ok()?.uid();
    let stat = std::fs::read_to_string(root.join("stat")).ok()?;
    let (ppid, pgid) = linux_parent_and_group(&stat)?;
    let executable = std::fs::read_link(root.join("exe")).ok()?;
    let command =
        String::from_utf8_lossy(&std::fs::read(root.join("cmdline")).ok()?).replace('\0', " ");
    Some(ProcessIdentity {
        pid,
        uid,
        ppid,
        pgid,
        executable,
        command,
    })
}

#[cfg(target_os = "macos")]
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let status = ProcessCommand::new("/bin/ps")
        .args(["-o", "uid=,ppid=,pgid=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    let fields = String::from_utf8_lossy(&status.stdout)
        .split_whitespace()
        .filter_map(|value| value.parse::<i64>().ok())
        .collect::<Vec<_>>();
    let [uid, ppid, pgid] = fields.as_slice() else {
        return None;
    };
    let executable = [Path::new("/usr/sbin/lsof"), Path::new("/usr/bin/lsof")]
        .into_iter()
        .find_map(|lsof| {
            let output = ProcessCommand::new(lsof)
                .args(["-a", "-p", &pid.to_string(), "-d", "txt", "-F", "n"])
                .stdin(Stdio::null())
                .output()
                .ok()?;
            output
                .status
                .success()
                .then_some(output.stdout)?
                .split(|byte| *byte == b'\n')
                .find_map(|line| line.strip_prefix(b"n"))
                .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
        })?;
    let command = ProcessCommand::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    Some(ProcessIdentity {
        pid,
        uid: *uid as u32,
        ppid: *ppid as u32,
        pgid: *pgid as i32,
        executable,
        command,
    })
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn process_identity(_pid: u32) -> Option<ProcessIdentity> {
    None
}

#[cfg(unix)]
fn same_executable(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    match (std::fs::metadata(left), std::fs::metadata(right)) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

#[cfg(unix)]
fn omp_ancestor(mut identity: ProcessIdentity, omp_executable: &Path) -> Option<ProcessIdentity> {
    for _ in 0..64 {
        if same_executable(&identity.executable, omp_executable) {
            return Some(identity);
        }
        if identity.ppid <= 1 || identity.ppid == identity.pid {
            return None;
        }
        identity = process_identity(identity.ppid)?;
    }
    None
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopTarget {
    Process(u32),
    Group(i32),
}

#[cfg(unix)]
fn stop_plan(files: &[PathBuf], omp_executable: &Path) -> Result<Vec<StopTarget>, HarnessError> {
    let mut writer_pids = Vec::new();
    for file in files {
        let pids = session_writer_pids(file).ok_or_else(|| {
            HarnessError::Protocol("Could not verify the process writing this OMP session".into())
        })?;
        for pid in pids {
            if !writer_pids.contains(&pid) {
                writer_pids.push(pid);
            }
        }
    }
    if writer_pids.is_empty() {
        return Ok(Vec::new());
    }

    // SAFETY: geteuid/getpgrp have no preconditions.
    let uid = unsafe { libc::geteuid() };
    let current_group = unsafe { libc::getpgrp() };
    let identities = writer_pids
        .iter()
        .map(|pid| process_identity(*pid))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            HarnessError::Protocol("An OMP writer exited while takeover was verified".into())
        })?;
    if identities.iter().any(|identity| identity.uid != uid) {
        return Err(HarnessError::Protocol(
            "The OMP session writer belongs to another user".into(),
        ));
    }

    // A tool can inherit OMP's append descriptor while moving into another
    // process group. Resolve the configured OMP executable through ancestry,
    // not only among the processes that still hold the descriptor. If OMP has
    // already died, accept only a same-user holder orphaned directly under PID
    // 1: the exact write-capable journal descriptor is then the surviving
    // ownership proof. A live unrelated process still fails closed.
    let mut roots = Vec::<ProcessIdentity>::new();
    let mut writer_roots = Vec::with_capacity(identities.len());
    for identity in &identities {
        match omp_ancestor(identity.clone(), omp_executable) {
            Some(root) => {
                if root.uid != uid {
                    return Err(HarnessError::Protocol(
                        "The OMP session writer belongs to another user".into(),
                    ));
                }
                writer_roots.push(Some(root.pid));
                if !roots.iter().any(|existing| existing.pid == root.pid) {
                    roots.push(root);
                }
            }
            None if identity.ppid == 1 => writer_roots.push(None),
            None => {
                return Err(HarnessError::Protocol(
                    "The write-capable holder is not OMP or an orphaned OMP tool".into(),
                ));
            }
        }
    }

    let mut targets = Vec::new();
    for root in &roots {
        let supervised_group = root.pgid > 1
            && root.pgid != current_group
            && (root.pgid == root.pid as i32
                || (root.ppid == root.pgid as u32
                    && process_identity(root.ppid).is_some_and(|supervisor| {
                        supervisor.command.contains(OMP_SUPERVISOR_MARKER)
                    })));
        let target = if supervised_group {
            StopTarget::Group(root.pgid)
        } else {
            StopTarget::Process(root.pid)
        };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    // Terminate inherited write holders before their OMP root. An orphaned
    // group leader is itself the only safely attributable root left, so stop
    // its isolated group; otherwise signal only the exact holder process.
    for (identity, root_pid) in identities.iter().zip(writer_roots).rev() {
        if root_pid == Some(identity.pid) {
            continue;
        }
        let target = if root_pid.is_none()
            && identity.pgid == identity.pid as i32
            && identity.pgid > 1
            && identity.pgid != current_group
        {
            StopTarget::Group(identity.pgid)
        } else {
            StopTarget::Process(identity.pid)
        };
        if !targets.contains(&target) {
            targets.insert(0, target);
        }
    }
    Ok(targets)
}

#[cfg(unix)]
fn signal_stop_plan(targets: &[StopTarget], signal: i32) -> Result<(), HarnessError> {
    for target in targets {
        let pid = match *target {
            StopTarget::Process(pid) => pid as i32,
            StopTarget::Group(group) => -group,
        };
        // SAFETY: targets were re-resolved from exact journal writers and
        // verified against uid, executable identity, ancestry, and group.
        if unsafe { libc::kill(pid, signal) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(HarnessError::Io(error));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn session_has_writer(files: &[PathBuf]) -> Result<bool, HarnessError> {
    let mut active = false;
    for file in files {
        match session_writer_state(file) {
            SessionWriterState::Active => active = true,
            SessionWriterState::Inactive => {}
            SessionWriterState::Unknown => {
                return Err(HarnessError::Protocol(
                    "Could not verify that the OMP session writer stopped".into(),
                ));
            }
        }
    }
    Ok(active)
}

#[cfg(unix)]
fn stop_session_writer(files: Vec<PathBuf>, omp_executable: PathBuf) -> Result<(), HarnessError> {
    let graceful = stop_plan(&files, &omp_executable)?;
    signal_stop_plan(&graceful, libc::SIGTERM)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if !session_has_writer(&files)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Rebuild from the still-open exact descriptors before escalation. PID
    // reuse or a newly introduced unrelated writer therefore fails closed.
    let forced = stop_plan(&files, &omp_executable)?;
    signal_stop_plan(&forced, libc::SIGKILL)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if !session_has_writer(&files)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(HarnessError::Protocol(
        "The OMP process did not release its session journal".into(),
    ))
}

fn valid_profile_name(profile: &str) -> bool {
    let profile = profile.trim();
    !profile.is_empty()
        && profile != "default"
        && profile != "."
        && profile != ".."
        && !profile.ends_with('.')
        && profile.len() <= 64
        && profile.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn omp_session_dirs(scaffold_host: bool) -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    let config_name = std::env::var_os("PI_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ".omp".into());
    let config_root = home.join(config_name);
    let profile = if scaffold_host {
        Some(SCAFFOLD_PROFILE.to_string())
    } else {
        std::env::var("OMP_PROFILE")
            .ok()
            .or_else(|| std::env::var("PI_PROFILE").ok())
            .filter(|value| valid_profile_name(value))
    };

    if profile.is_none()
        && let Some(agent_dir) =
            std::env::var_os("PI_CODING_AGENT_DIR").filter(|value| !value.is_empty())
    {
        return vec![PathBuf::from(agent_dir).join("sessions")];
    }

    let mut dirs = Vec::with_capacity(2);
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        let xdg_root = PathBuf::from(xdg).join("omp");
        let xdg_profile_root = profile.as_deref().map_or_else(
            || xdg_root.clone(),
            |name| xdg_root.join("profiles").join(name),
        );
        if xdg_profile_root.is_dir() {
            dirs.push(xdg_profile_root.join("sessions"));
        }
    }
    let agent_dir = profile.as_deref().map_or_else(
        || config_root.join("agent"),
        |name| config_root.join("profiles").join(name).join("agent"),
    );
    dirs.push(agent_dir.join("sessions"));
    dirs
}

fn omp_agent_dir(scaffold_host: bool) -> PathBuf {
    omp_session_dirs(scaffold_host)
        .into_iter()
        .next()
        .and_then(|sessions| sessions.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from(".omp/agent"))
}

fn configure_inference_gateway(
    command: &mut Command,
    agent_dir: &Path,
    inference: &InferenceRoute,
) -> Result<(), HarnessError> {
    let provider = crate::auth_gateway::provider(&inference.provider).ok_or_else(|| {
        HarnessError::Protocol(format!(
            "unsupported shared inference provider: {}",
            inference.provider
        ))
    })?;
    if let Some(extension) = crate::auth_gateway::install_extension(agent_dir)? {
        command.arg("--extension").arg(extension);
    }
    let model = inference
        .model
        .rsplit_once('/')
        .map_or(inference.model.as_str(), |(_, model)| model);
    command.args(["--model", &format!("{provider}/{model}")]);
    Ok(())
}

fn session_file_has_id(path: &Path, session_id: &str) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file.take(OMP_SESSION_HEADER_BYTES));
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|bytes| bytes > 0) {
        if let Ok(value) = serde_json::from_str::<Value>(&line)
            && value.get("type").and_then(Value::as_str) == Some("session")
        {
            return value.get("id").and_then(Value::as_str) == Some(session_id);
        }
        line.clear();
    }
    false
}

fn matching_session_files(session_dirs: &[PathBuf], session_id: &str) -> (Vec<PathBuf>, bool) {
    matching_session_files_with_limit(session_dirs, session_id, OMP_SESSION_DIRECTORY_SCAN_LIMIT)
}

fn matching_session_files_with_limit(
    session_dirs: &[PathBuf],
    session_id: &str,
    directory_limit: usize,
) -> (Vec<PathBuf>, bool) {
    let direct = Path::new(session_id);
    if direct.is_file() {
        return (vec![direct.to_path_buf()], true);
    }

    // Bound directory traversal, not directory entries. OMP keeps many session
    // journals beside a small number of cwd directories, so counting files can
    // exhaust the budget before reaching the directory that owns the requested
    // session. Every regular file in each visited directory must remain
    // eligible for an exact header match.
    let mut pending: VecDeque<PathBuf> = session_dirs.iter().cloned().collect();
    let mut visited_directories = 0;
    let mut matches = Vec::new();
    while let Some(dir) = pending.pop_front() {
        if visited_directories >= directory_limit {
            return (matches, false);
        }
        visited_directories += 1;
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                pending.push_back(path);
            } else if file_type.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
                && session_file_has_id(&path, session_id)
            {
                matches.push(path);
            }
        }
    }
    (matches, true)
}

fn resolve_omp_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OMP_EXECUTABLE").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let executable = if cfg!(windows) { "omp.exe" } else { "omp" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(executable))
                .collect()
        })
        .unwrap_or_default();
    if let Some(shell_path) = crate::shell_env::login_shell_path() {
        candidates.extend(std::env::split_paths(shell_path).map(|dir| dir.join(executable)));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/bin").join(executable));
    }
    candidates.push(PathBuf::from("/usr/local/bin").join(executable));
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|dir| dir.join(executable)),
    );
    candidates.into_iter().find(|path| path.exists())
}

/// The installed `omp` executable, if any — the engine's update inventory
/// resolves through the same candidate chain runs use.
pub fn installed_executable() -> Option<PathBuf> {
    resolve_omp_executable()
}

#[derive(Clone, Default)]
struct AuthBrokerEnvironment {
    url: Option<OsString>,
    token: Option<OsString>,
    token_file: Option<OsString>,
}

impl AuthBrokerEnvironment {
    fn from_process() -> Self {
        Self {
            url: std::env::var_os(AUTH_BROKER_URL_ENV),
            token: std::env::var_os(AUTH_BROKER_TOKEN_ENV),
            token_file: std::env::var_os(AUTH_BROKER_TOKEN_FILE_ENV),
        }
    }
}

fn propagate_auth_broker_environment(
    command: &mut Command,
    scaffold_host: bool,
    include_token: bool,
    environment: &AuthBrokerEnvironment,
) {
    command.env_remove(AUTH_BROKER_TOKEN_FILE_ENV);
    if scaffold_host {
        for name in [
            AUTH_BROKER_TOKEN_ENV,
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
        ] {
            command.env_remove(name);
        }
    }
    if !include_token {
        command.env_remove(AUTH_BROKER_TOKEN_ENV);
    }
    if let Some(url) = environment.url.as_ref().filter(|value| !value.is_empty()) {
        command.env(AUTH_BROKER_URL_ENV, url);
    }
    if !include_token {
        return;
    }
    // Local controllers may inherit a workstation broker token. A scoped host
    // must never accept that long-lived credential and can use only its
    // single-use token file projection.
    if !scaffold_host
        && let Some(token) = environment.token.as_ref().filter(|value| !value.is_empty())
    {
        command.env(AUTH_BROKER_TOKEN_ENV, token);
        return;
    }
    let Some(path) = environment
        .token_file
        .as_ref()
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    // OMP 17.2.9 consumes OMP_AUTH_BROKER_TOKEN. Comet accepts a mode-0600,
    // single-use file so supervisors never place the bearer in argv or their
    // long-lived environment. Remove it before parsing or spawning on every
    // outcome, including malformed content and permission failures.
    let metadata = std::fs::symlink_metadata(path);
    let bytes = metadata.as_ref().ok().and_then(|metadata| {
        if !metadata.is_file() || metadata.len() > 16 * 1024 {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                return None;
            }
        }
        std::fs::read(path).ok()
    });
    let _ = std::fs::remove_file(path);
    if let Some(token) = bytes
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        command.env(AUTH_BROKER_TOKEN_ENV, token);
    }
}

pub struct OmpHarness {
    executable: Option<PathBuf>,
    supervisor_executable: Option<PathBuf>,
    scaffold_host: bool,
    interrupt_grace: Duration,
    session_dirs: Option<Vec<PathBuf>>,
    session_writer_probe: Option<PathBuf>,
    auth_broker_environment: Option<AuthBrokerEnvironment>,
}

impl Default for OmpHarness {
    fn default() -> Self {
        Self {
            executable: None,
            supervisor_executable: None,
            scaffold_host: false,
            interrupt_grace: Duration::from_secs(2),
            session_dirs: None,
            session_writer_probe: None,
            auth_broker_environment: None,
        }
    }
}

impl OmpHarness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn scaffold_host() -> Self {
        Self {
            scaffold_host: true,
            ..Self::default()
        }
    }

    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = Some(executable.into());
        self
    }

    /// Wrap persistent OMP runs in this Comet executable's hidden supervisor.
    pub fn with_supervisor_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.supervisor_executable = Some(executable.into());
        self
    }

    /// Override the graceful interrupt deadline for deterministic integration tests.
    #[doc(hidden)]
    pub fn with_interrupt_grace(mut self, interrupt_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self
    }

    #[cfg(test)]
    fn with_auth_broker_environment(mut self, environment: AuthBrokerEnvironment) -> Self {
        self.auth_broker_environment = Some(environment);
        self
    }

    /// Override OMP session discovery for deterministic integration tests.
    #[doc(hidden)]
    pub fn with_session_dir(mut self, session_dir: impl Into<PathBuf>) -> Self {
        self.session_dirs = Some(vec![session_dir.into()]);
        self
    }

    /// Override the exact-path open-file probe for deterministic integration tests.
    #[doc(hidden)]
    pub fn with_session_writer_probe(mut self, executable: impl Into<PathBuf>) -> Self {
        self.session_writer_probe = Some(executable.into());
        self
    }

    fn writer_state(&self, path: &Path) -> SessionWriterState {
        self.session_writer_probe.as_deref().map_or_else(
            || session_writer_state(path),
            |probe| session_writer_state_with(path, probe).unwrap_or(SessionWriterState::Unknown),
        )
    }

    fn ensure_resume_has_no_writer(&self, resume: Option<&str>) -> Result<(), HarnessError> {
        let Some(session_id) = resume else {
            return Ok(());
        };
        let session_dirs = self
            .session_dirs
            .clone()
            .unwrap_or_else(|| omp_session_dirs(self.scaffold_host));
        let (files, exhaustive) = matching_session_files(&session_dirs, session_id);
        if files.is_empty() {
            return if exhaustive {
                Ok(())
            } else {
                Err(HarnessError::Protocol(
                    "Could not verify that this OMP session is inactive, so it was not resumed."
                        .into(),
                ))
            };
        }
        let mut state = SessionWriterState::Inactive;
        for file in files {
            match self.writer_state(&file) {
                SessionWriterState::Active => {
                    return Err(HarnessError::SessionBusy {
                        session_id: session_id.to_string(),
                    });
                }
                SessionWriterState::Unknown => state = SessionWriterState::Unknown,
                SessionWriterState::Inactive => {}
            }
        }
        if state == SessionWriterState::Unknown {
            return Err(HarnessError::Protocol(
                "Could not verify that this OMP session is inactive, so it was not resumed.".into(),
            ));
        }
        Ok(())
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        self.executable.clone().or_else(resolve_omp_executable).ok_or_else(|| {
            HarnessError::NotInstalled("omp (searched OMP_EXECUTABLE, PATH, login-shell PATH, ~/.local/bin, /usr/local/bin, and Node manager bins)".into())
        })
    }

    fn base_command(
        &self,
        executable: &Path,
        cwd: &str,
        include_auth_broker_token: bool,
        supervised: bool,
    ) -> Command {
        let mut command = if supervised {
            self.supervisor_executable.as_ref().map_or_else(
                || Command::new(executable),
                |supervisor| {
                    let mut command = Command::new(supervisor);
                    command
                        .arg(OMP_SUPERVISOR_MARKER)
                        .arg(std::process::id().to_string())
                        .arg(executable);
                    command
                },
            )
        } else {
            Command::new(executable)
        };
        crate::compose_child_path(&mut command, executable);
        let auth_broker_environment = self
            .auth_broker_environment
            .clone()
            .unwrap_or_else(AuthBrokerEnvironment::from_process);
        propagate_auth_broker_environment(
            &mut command,
            self.scaffold_host,
            include_auth_broker_token,
            &auth_broker_environment,
        );
        if !cwd.is_empty() {
            command.current_dir(cwd);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// The persistent RPC-mode child that owns a run: JSONL frames on stdio,
    /// hardened the same way the ACP lane was (yolo approvals; scaffold hosts
    /// run the isolated profile with discovery disabled).
    fn rpc_mode_command(
        &self,
        executable: &Path,
        cwd: &str,
        include_auth_broker_token: bool,
    ) -> Command {
        let mut command = self.base_command(executable, cwd, include_auth_broker_token, false);
        // OMP sets CI on tool children independently of its launch environment.
        // Repository-owned wrappers use this local-only marker to recover local
        // command semantics; Scaffold explicitly omits it and stays CI.
        self.configure_rpc_mode(&mut command);
        command
    }

    fn configure_rpc_mode(&self, command: &mut Command) {
        if self.scaffold_host {
            command.env("CI", "true");
            command.env_remove(LOCAL_RUNTIME_ENV);
        } else {
            command.env("CI", "false");
            command.env(LOCAL_RUNTIME_ENV, "1");
        }
        command.args(["--mode", "rpc", "--approval-mode", "yolo"]);
        if self.scaffold_host {
            command.args([
                "--profile",
                SCAFFOLD_PROFILE,
                "--no-extensions",
                "--no-skills",
                "--no-rules",
            ]);
        }
    }

    /// A run's child command: model, thinking level, and resume are pinned as
    /// CLI flags so the session opens fully configured — RPC mode needs no
    /// in-band configure stage.
    fn run_command(&self, executable: &Path, request: &RunRequest) -> Command {
        let mut command = self.base_command(executable, &request.cwd, true, true);
        self.configure_rpc_mode(&mut command);
        if !self.scaffold_host
            && let Some(model) = request.model.as_deref().filter(|model| *model != "default")
        {
            command.args(["--model", model]);
        }
        if let Some(reasoning) = request.reasoning {
            command.args(["--thinking", thinking_flag(reasoning)]);
        }
        if let Some(resume) = request.resume.as_deref() {
            command.args(["--resume", resume]);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.as_std_mut().process_group(0);
        }
        command
    }

    fn command_catalog_command(&self, executable: &Path, cwd: &str) -> Command {
        let mut command = self.rpc_mode_command(executable, cwd, false);
        command.arg("--no-session");
        command
    }
}

fn omp_config_command(cwd: &str) -> Result<Command, HarnessError> {
    let executable = resolve_omp_executable().ok_or_else(|| {
        HarnessError::NotInstalled(
            "omp (searched OMP_EXECUTABLE, PATH, login-shell PATH, ~/.local/bin, \
             /usr/local/bin, and Node manager bins)"
                .into(),
        )
    })?;
    let mut command = Command::new(&executable);
    crate::compose_child_path(&mut command, &executable);
    if !cwd.is_empty() {
        command.current_dir(cwd);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

async fn run_omp_config(cwd: &str, args: &[&str]) -> Result<Value, HarnessError> {
    let output = omp_config_command(cwd)?.args(args).output().await?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(HarnessError::Protocol(if message.is_empty() {
            format!("OMP config command failed with {}", output.status)
        } else {
            message
        }));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| {
        HarnessError::Protocol(format!("OMP config returned invalid JSON: {error}"))
    })
}

fn parse_advisor_config(settings: &Value) -> Result<OmpAdvisorConfig, HarnessError> {
    let value = |key: &str| settings.get(key).and_then(|setting| setting.get("value"));
    let enabled = value("advisor.enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let subagents = value("advisor.subagents")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sync_backlog = match value("advisor.syncBacklog")
        .and_then(Value::as_str)
        .unwrap_or("off")
    {
        "off" => OmpAdvisorSyncBacklog::Off,
        "1" => OmpAdvisorSyncBacklog::One,
        "3" => OmpAdvisorSyncBacklog::Three,
        "5" => OmpAdvisorSyncBacklog::Five,
        other => {
            return Err(HarnessError::Protocol(format!(
                "OMP returned an unsupported advisor.syncBacklog value: {other}"
            )));
        }
    };
    let immune_turns = value("advisor.immuneTurns")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(3);
    let model = value("modelRoles")
        .and_then(Value::as_object)
        .and_then(|roles| roles.get("advisor"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(OmpAdvisorConfig {
        enabled,
        model,
        subagents,
        sync_backlog,
        immune_turns,
    })
}

pub async fn read_advisor_config(cwd: &str) -> Result<OmpAdvisorConfig, HarnessError> {
    let settings = run_omp_config(cwd, &["config", "list", "--json"]).await?;
    parse_advisor_config(&settings)
}

pub async fn update_advisor_config(
    cwd: &str,
    update: AdvisorConfigUpdate,
) -> Result<OmpAdvisorConfig, HarnessError> {
    let (key, value) = match update {
        AdvisorConfigUpdate::Enabled(value) => ("advisor.enabled", value.to_string()),
        AdvisorConfigUpdate::Subagents(value) => ("advisor.subagents", value.to_string()),
        AdvisorConfigUpdate::SyncBacklog(value) => {
            ("advisor.syncBacklog", value.value().to_string())
        }
        AdvisorConfigUpdate::ImmuneTurns(value) => ("advisor.immuneTurns", value.to_string()),
        AdvisorConfigUpdate::Model(model) => {
            let model = model.trim();
            if model.is_empty() {
                return Err(HarnessError::Protocol(
                    "Advisor model cannot be empty".into(),
                ));
            }
            let current = run_omp_config(cwd, &["config", "get", "modelRoles", "--json"]).await?;
            let mut roles = current
                .get("value")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            roles.insert("advisor".into(), Value::String(model.to_string()));
            ("modelRoles", Value::Object(roles).to_string())
        }
    };
    run_omp_config(cwd, &["config", "set", key, &value, "--json"]).await?;
    read_advisor_config(cwd).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OmpCatalog {
    models: Vec<OmpCatalogModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OmpCatalogModel {
    selector: String,
    name: String,
    context_window: Option<u64>,
    max_tokens: Option<u64>,
    #[serde(default)]
    thinking: Option<Vec<String>>,
}

fn models_from_catalog(bytes: &[u8]) -> Result<Vec<Model>, HarnessError> {
    let catalog: OmpCatalog = serde_json::from_slice(bytes).map_err(|error| {
        HarnessError::Protocol(format!("OMP model catalog was not valid JSON: {error}"))
    })?;
    Ok(catalog
        .models
        .into_iter()
        .map(|model| {
            let size_description = match (model.context_window, model.max_tokens) {
                (Some(context_window), Some(max_tokens)) => {
                    format!("{context_window} context · {max_tokens} max output · ")
                }
                (Some(context_window), None) => format!("{context_window} context · "),
                (None, Some(max_tokens)) => format!("{max_tokens} max output · "),
                (None, None) => String::new(),
            };
            Model {
                id: model.selector,
                label: model.name,
                description: Some(format!(
                    "{size_description}Listed in OMP's catalog; run availability is not verified; authorization is not verified",
                )),
                reasoning_levels: model
                    .thinking
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|level| serde_json::from_value(Value::String(level)).ok())
                    .collect(),
                options: Vec::new(),
            }
        })
        .collect())
}

#[async_trait]
impl Harness for OmpHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Omp
    }
    fn display_name(&self) -> &str {
        "OMP"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// RPC-mode prompts carry `streamingBehavior: "steer"`: OMP checks
    /// steering between tool calls (default `interruptMode: "immediate"`), so
    /// followups alter the live turn instead of queueing behind it.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn stop_session(&self, session_id: &str) -> Result<(), HarnessError> {
        let session_dirs = self
            .session_dirs
            .clone()
            .unwrap_or_else(|| omp_session_dirs(self.scaffold_host));
        let (files, exhaustive) = matching_session_files(&session_dirs, session_id);
        if files.is_empty() || !exhaustive {
            return Err(HarnessError::Protocol(
                "Could not resolve the exact OMP session journal for takeover".into(),
            ));
        }
        let executable = self.resolve_executable()?;
        #[cfg(unix)]
        return tokio::task::spawn_blocking(move || stop_session_writer(files, executable))
            .await
            .map_err(|error| {
                HarnessError::Protocol(format!("OMP takeover worker failed: {error}"))
            })?;
        #[cfg(not(unix))]
        {
            let _ = (files, executable);
            Err(HarnessError::Protocol(
                "Stopping an external OMP session requires a Unix host".into(),
            ))
        }
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let executable = self.resolve_executable()?;
        let output = self
            .base_command(&executable, "", false, false)
            .args(["models", "--json"])
            .stdin(Stdio::null())
            .output()
            .await?;
        if !output.status.success() {
            let tail = crate::StderrTail::default();
            for line in String::from_utf8_lossy(&output.stderr).lines() {
                tail.push(line);
            }
            return Err(HarnessError::Protocol(crate::crash_message(
                "OMP model catalog",
                Some(output.status),
                &tail,
            )));
        }
        models_from_catalog(&output.stdout)
    }

    async fn commands(&self, cwd: &str) -> Result<Vec<HarnessCommand>, HarnessError> {
        let executable = self.resolve_executable()?;
        let mut child = self
            .command_catalog_command(&executable, cwd)
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    HarnessError::NotInstalled(executable.display().to_string())
                } else {
                    HarnessError::Io(error)
                }
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("OMP RPC child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("OMP RPC child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tail.push(&line);
                }
            });
        }
        let (client, frames) = rpc_mode::RpcModeClient::new(stdin, stdout);
        rpc_mode::command_catalog(
            rpc_mode::RpcModeProcess {
                child,
                client,
                frames,
                stderr_tail,
                process_group: None,
                process_group_guard: rpc_mode::ProcessGroupGuard::new(None),
                run_config: None,
            },
            RPC_COMMAND_CATALOG_DEADLINE,
        )
        .await
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let executable = self.resolve_executable()?;
        self.ensure_resume_has_no_writer(request.resume.as_deref())?;
        let mut command = self.run_command(&executable, &request);
        let run_config = OmpRunConfig::create()?;
        run_config.apply(&mut command)?;
        crate::apply_run_context(&mut command, controls.context.as_ref());
        if let Some(inference) = controls
            .context
            .as_ref()
            .and_then(|context| context.inference.as_ref())
        {
            configure_inference_gateway(
                &mut command,
                &omp_agent_dir(self.scaffold_host),
                inference,
            )?;
        } else if self.scaffold_host {
            configure_scaffold_inference_profile(&mut command, request.model.as_deref())?;
        }
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(executable.display().to_string())
            } else {
                HarnessError::Io(error)
            }
        })?;
        #[cfg(unix)]
        let process_group = child.id().map(|pid| pid as i32);
        #[cfg(not(unix))]
        let process_group = None;
        let process_group_guard = rpc_mode::ProcessGroupGuard::new(process_group);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("OMP RPC child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("OMP RPC child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tail.push(&line);
                }
            });
        }
        let (client, frames) = rpc_mode::RpcModeClient::new(stdin, stdout);
        let (events, receiver) = mpsc::channel(256);
        let interrupt_grace = self.interrupt_grace;
        let expected_resume = request.resume.clone();
        tokio::spawn(async move {
            if let Err(error) = rpc_mode::run_rpc(
                rpc_mode::RpcModeProcess {
                    child,
                    client,
                    frames,
                    stderr_tail,
                    process_group,
                    process_group_guard,
                    run_config: Some(run_config),
                },
                request,
                controls,
                events.clone(),
                interrupt_grace,
                rpc_mode::RpcRunOptions {
                    process_label: "OMP RPC",
                    expected_resume,
                },
            )
            .await
            {
                let _ = events.send(Err(error)).await;
            }
        });
        Ok(
            futures::stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|event| (event, receiver))
            })
            .boxed(),
        )
    }
}

fn advertised_config_id(state: &Value, candidates: &[&str]) -> Option<String> {
    state
        .get("configOptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| {
            option
                .get("id")
                .or_else(|| option.get("configId"))
                .and_then(Value::as_str)
        })
        .find(|id| candidates.contains(id))
        .map(str::to_string)
}

async fn set_config_option(
    client: &RpcClient,
    session_id: &str,
    config_id: &str,
    value: Value,
) -> Result<(), HarnessError> {
    client
        .request(
            "session/set_config_option",
            json!({
                "sessionId": session_id,
                "configId": config_id,
                "value": value,
            }),
        )
        .await?;
    Ok(())
}

async fn configure_session(
    client: &RpcClient,
    session_id: &str,
    state: &Value,
    request: &RunRequest,
) -> Result<(), HarnessError> {
    if let Some(model) = request.model.as_deref().filter(|model| *model != "default") {
        let config_id = advertised_config_id(state, &["model"]).ok_or_else(|| {
            HarnessError::Protocol("OMP ACP did not advertise model configuration".into())
        })?;
        set_config_option(
            client,
            session_id,
            &config_id,
            Value::String(model.to_string()),
        )
        .await?;
    }
    if let Some(reasoning) = request.reasoning {
        let config_id =
            advertised_config_id(state, &["thinking", "reasoning"]).ok_or_else(|| {
                HarnessError::Protocol("OMP ACP did not advertise reasoning configuration".into())
            })?;
        let value = serde_json::to_value(reasoning).map_err(|error| {
            HarnessError::Protocol(format!("Could not encode OMP reasoning: {error}"))
        })?;
        set_config_option(client, session_id, &config_id, value).await?;
    }
    for (option_id, value) in &request.model_options {
        if advertised_config_id(state, &[option_id.as_str()]).is_some() {
            set_config_option(client, session_id, option_id, value.clone()).await?;
        }
    }
    Ok(())
}
pub(crate) struct AcpProcess {
    child: Child,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    stderr_tail: crate::StderrTail,
}

impl AcpProcess {
    pub(crate) fn new(
        child: Child,
        client: RpcClient,
        incoming: mpsc::Receiver<Incoming>,
        stderr_tail: crate::StderrTail,
    ) -> Self {
        Self {
            child,
            client,
            incoming,
            stderr_tail,
        }
    }
}

pub(crate) struct AcpRunOptions {
    pub harness: HarnessId,
    pub process_label: &'static str,
    pub preloaded_session_id: Option<String>,
    pub reported_session_dir: Option<PathBuf>,
    pub configure_session: bool,
    pub persistent: bool,
}

fn stage_timeout_message(process_label: &str, stage: &str, deadline: Duration) -> String {
    format!(
        "{process_label} {stage} timed out after {}s",
        deadline.as_secs()
    )
}

async fn run_stage<T, F>(
    process_label: &str,
    stage: &str,
    deadline: Duration,
    future: F,
) -> Result<T, HarnessError>
where
    F: Future<Output = Result<T, HarnessError>>,
{
    tracing::debug!(
        target: "comet_harness::omp",
        process = process_label,
        stage = stage,
        timeout_ms = deadline.as_millis() as u64,
        "ACP stage started"
    );
    match tokio::time::timeout(deadline, future).await {
        Ok(Ok(value)) => {
            tracing::debug!(
                target: "comet_harness::omp",
                process = process_label,
                stage = stage,
                "ACP stage completed"
            );
            Ok(value)
        }
        Ok(Err(error)) => {
            tracing::warn!(
                target: "comet_harness::omp",
                process = process_label,
                stage = stage,
                error = %error,
                "ACP stage failed"
            );
            Err(error)
        }
        Err(_) => {
            let message = stage_timeout_message(process_label, stage, deadline);
            tracing::warn!(
                target: "comet_harness::omp",
                process = process_label,
                stage = stage,
                error = %message,
                "ACP stage timed out"
            );
            Err(HarnessError::Protocol(message))
        }
    }
}

async fn wait_for_reported_session(dir: &Path) -> Option<String> {
    for _ in 0..50 {
        let newest = std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let is_session = matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("json" | "jsonl")
                );
                if !is_session {
                    return None;
                }
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((modified, path))
            })
            .max_by_key(|(modified, _)| *modified)
            .map(|(_, path)| path.to_string_lossy().into_owned());
        if newest.is_some() {
            return newest;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

pub(crate) async fn run_acp(
    process: AcpProcess,
    request: RunRequest,
    controls: RunControls,
    events: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    interrupt_grace: Duration,
    options: AcpRunOptions,
) -> Result<(), HarnessError> {
    let AcpProcess {
        mut child,
        client,
        mut incoming,
        stderr_tail,
    } = process;
    let initialized = run_stage(
        options.process_label,
        "initialize",
        ACP_INITIALIZE_DEADLINE,
        client.request(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {
                    "elicitation": { "form": {}, "url": {} }
                },
                "clientInfo": { "name": "ashler-comet", "version": env!("CARGO_PKG_VERSION") }
            }),
        ),
    )
    .await?;
    if initialized.get("protocolVersion").and_then(Value::as_i64) != Some(ACP_PROTOCOL_VERSION) {
        return Err(HarnessError::Protocol(format!(
            "{} protocol version mismatch — this OMP and Comet are out of step; \
             run `omp update` (or update it from Settings → Agents) or update Comet",
            options.process_label
        )));
    }

    let (session_id, reported_session_id, session_state) =
        if let Some(reported_session_id) = options.preloaded_session_id.clone() {
            let state = run_stage(
                options.process_label,
                "session/new",
                ACP_SESSION_DEADLINE,
                client.request(
                    "session/new",
                    json!({ "cwd": request.cwd, "mcpServers": [] }),
                ),
            )
            .await?;
            let session_id = state
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    HarnessError::Protocol(format!(
                        "{} session/new response had no sessionId",
                        options.process_label
                    ))
                })?
                .to_string();
            (session_id, reported_session_id, state)
        } else if let Some(session_id) = request.resume.as_deref() {
            let method = if initialized
                .pointer("/agentCapabilities/loadSession")
                .and_then(Value::as_bool)
                == Some(true)
            {
                "session/load"
            } else if initialized
                .pointer("/agentCapabilities/sessionCapabilities/resume")
                .is_some()
            {
                "session/resume"
            } else {
                return Err(HarnessError::Protocol(format!(
                    "{} did not advertise session load or resume",
                    options.process_label
                )));
            };
            let state = run_stage(
                options.process_label,
                method,
                ACP_SESSION_DEADLINE,
                client.request(
                    method,
                    json!({ "sessionId": session_id, "cwd": request.cwd, "mcpServers": [] }),
                ),
            )
            .await?;
            (session_id.to_string(), session_id.to_string(), state)
        } else {
            let state = run_stage(
                options.process_label,
                "session/new",
                ACP_SESSION_DEADLINE,
                client.request(
                    "session/new",
                    json!({ "cwd": request.cwd, "mcpServers": [] }),
                ),
            )
            .await?;
            let session_id = state
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    HarnessError::Protocol(format!(
                        "{} session/new response had no sessionId",
                        options.process_label
                    ))
                })?
                .to_string();
            (session_id.clone(), session_id, state)
        };
    if options.configure_session {
        run_stage(
            options.process_label,
            "session/configure",
            ACP_CONFIGURE_DEADLINE,
            configure_session(&client, &session_id, &session_state, &request),
        )
        .await?;
    }
    let reported_session_id = if options.preloaded_session_id.is_none() {
        if let Some(dir) = options.reported_session_dir.as_deref() {
            wait_for_reported_session(dir).await.ok_or_else(|| {
                HarnessError::Protocol(format!(
                    "{} session/report timed out waiting for a durable session file",
                    options.process_label
                ))
            })?
        } else {
            reported_session_id
        }
    } else {
        reported_session_id
    };
    let message_id_run_nonce = uuid::Uuid::new_v4();
    let mut turn_number = 0_u64;
    let mut assistant_message_id =
        acp_assistant_message_id(&session_id, &message_id_run_nonce, turn_number);
    if events
        .send(Ok(AgentEvent::SessionStarted {
            harness: options.harness,
            model: request.model.clone().unwrap_or_else(|| "default".into()),
            tools: vec![],
            cwd: request.cwd.clone(),
            session_id: reported_session_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        }))
        .await
        .is_err()
    {
        return Ok(());
    }

    let mut first_prompt = Vec::with_capacity(request.attachments.len() + 1);
    for attachment in &request.attachments {
        let bytes = std::fs::read(attachment)?;
        let mime_type = match Path::new(attachment)
            .extension()
            .and_then(|value| value.to_str())
        {
            Some("png") => "image/png",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => "image/jpeg",
        };
        first_prompt.push(json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            "mimeType": mime_type,
            "uri": format!("file://{attachment}")
        }));
    }
    first_prompt.push(json!({ "type": "text", "text": request.prompt }));

    let RunControls {
        request_input,
        mut steering,
        interrupt,
        context: _,
    } = controls;
    let mut queued_prompts = VecDeque::new();
    let mut steering_open = true;
    let mut prompt_blocks = first_prompt;
    let mut acp_closed = false;

    loop {
        tracing::debug!(
            target: "comet_harness::omp",
            process = options.process_label,
            stage = "session/prompt",
            turn = turn_number,
            inactivity_log_interval_ms = ACP_PROMPT_INACTIVITY_LOG_INTERVAL.as_millis() as u64,
            "ACP stage started"
        );
        let prompt = client.request(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": prompt_blocks }),
        );
        tokio::pin!(prompt);
        let inactivity = tokio::time::sleep(ACP_PROMPT_INACTIVITY_LOG_INTERVAL);
        tokio::pin!(inactivity);

        let response = loop {
            tokio::select! {
                // Biased: a delivered final response must win over the EOF
                // already queued behind it — agents that exit right after
                // `end_turn` are ending cleanly, not crashing.
                biased;
                response = &mut prompt => match response {
                    Ok(response) => break response,
                    Err(error) => {
                        // Prefer the richer crash context (exit status +
                        // stderr tail) when the child is already gone.
                        let error = match child.try_wait() {
                            Ok(Some(status)) => HarnessError::Protocol(crate::crash_message(
                                options.process_label,
                                Some(status),
                                &stderr_tail,
                            )),
                            _ => error,
                        };
                        tracing::warn!(
                            target: "comet_harness::omp",
                            process = options.process_label,
                            stage = "session/prompt",
                            error = %error,
                            "ACP stage failed"
                        );
                        return Err(error);
                    }
                },
                steer = steering.recv(), if steering_open => match steer {
                    Some(steer) => queued_prompts.push_back(steer.prompt),
                    None => steering_open = false,
                },
                _ = interrupt.cancelled() => {
                    client.notify("session/cancel", Some(json!({ "sessionId": session_id })));
                    let _ = tokio::time::timeout(interrupt_grace, child.wait()).await;
                    let _ = child.kill().await;
                    events.send(Ok(AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: Some(reported_session_id.clone()),
                    })).await.ok();
                    return Ok(());
                }
                item = incoming.recv() => {
                    inactivity.as_mut().reset(Instant::now() + ACP_PROMPT_INACTIVITY_LOG_INTERVAL);
                    match item {
                        Some(item) => {
                            if !handle_session_incoming(
                                item,
                                &client,
                                options.harness,
                                &request_input,
                                &events,
                            )
                            .await
                            {
                                return Err(HarnessError::Protocol(crate::crash_message(
                                    options.process_label,
                                    child.try_wait().ok().flatten(),
                                    &stderr_tail,
                                )));
                            }
                        }
                        None => {
                            return Err(HarnessError::Protocol(crate::crash_message(
                                options.process_label,
                                child.try_wait().ok().flatten(),
                                &stderr_tail,
                            )));
                        }
                    }
                }
                _ = &mut inactivity => {
                    tracing::warn!(
                        target: "comet_harness::omp",
                        process = options.process_label,
                        stage = "session/prompt",
                        inactivity_ms = ACP_PROMPT_INACTIVITY_LOG_INTERVAL.as_millis() as u64,
                        "ACP prompt remains active without notifications"
                    );
                    inactivity
                        .as_mut()
                        .reset(Instant::now() + ACP_PROMPT_INACTIVITY_LOG_INTERVAL);
                }
                _ = events.closed() => {
                    let _ = child.kill().await;
                    return Ok(());
                }
            }
        };

        while let Ok(item) = incoming.try_recv() {
            if !handle_session_incoming(item, &client, options.harness, &request_input, &events)
                .await
            {
                acp_closed = true;
                break;
            }
        }
        tracing::debug!(
            target: "comet_harness::omp",
            process = options.process_label,
            stage = "session/prompt",
            turn = turn_number,
            "ACP stage completed"
        );
        if let Some(usage) = response.get("usage") {
            events
                .send(Ok(AgentEvent::Usage {
                    input_tokens: usage
                        .get("inputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    output_tokens: usage
                        .get("outputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                }))
                .await
                .ok();
        }
        let reason = response
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn");
        let status = if reason == "cancelled" {
            DoneStatus::Interrupted
        } else {
            DoneStatus::Completed
        };
        if events
            .send(Ok(AgentEvent::Done {
                status,
                result: None,
                error: None,
                session_id: Some(reported_session_id.clone()),
            }))
            .await
            .is_err()
        {
            let _ = child.kill().await;
            return Ok(());
        }
        if !options.persistent {
            return Ok(());
        }
        if acp_closed {
            // The agent closed its stream after finishing the turn (end_turn
            // received, Done delivered). Some ACP agents exit per conversation
            // even when we run them persistently — a clean end, not a crash.
            tracing::debug!(
                target: "comet_harness::omp",
                process = options.process_label,
                "ACP stream closed after a completed turn; ending persistent session"
            );
            return Ok(());
        }

        let next_prompt = loop {
            if let Some(prompt) = queued_prompts.pop_front() {
                break prompt;
            }
            if !steering_open {
                let _ = child.kill().await;
                return Ok(());
            }
            tokio::select! {
                steer = steering.recv() => match steer {
                    Some(steer) => break steer.prompt,
                    None => steering_open = false,
                },
                _ = interrupt.cancelled() => {
                    let _ = child.kill().await;
                    events.send(Ok(AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: Some(reported_session_id.clone()),
                    })).await.ok();
                    return Ok(());
                }
                item = incoming.recv() => match item {
                    Some(Incoming::Eof) | None => {
                        // Stream closed while idle between turns: the previous
                        // turn completed, so this is the same clean per-turn
                        // exit as above — never a crash report.
                        tracing::debug!(
                            target: "comet_harness::omp",
                            process = options.process_label,
                            "ACP stream closed between turns; ending persistent session"
                        );
                        return Ok(());
                    }
                    Some(item) => {
                        if !handle_session_incoming(
                            item,
                            &client,
                            options.harness,
                            &request_input,
                            &events,
                        )
                        .await
                        {
                            return Err(HarnessError::Protocol(crate::crash_message(
                                options.process_label,
                                child.try_wait().ok().flatten(),
                                &stderr_tail,
                            )));
                        }
                    }
                },
                _ = events.closed() => {
                    let _ = child.kill().await;
                    return Ok(());
                }
            }
        };

        turn_number += 1;
        let next_assistant_message_id =
            acp_assistant_message_id(&session_id, &message_id_run_nonce, turn_number);
        if events
            .send(Ok(AgentEvent::Steered {
                assistant_message_id: Some(assistant_message_id),
                next_assistant_message_id: Some(next_assistant_message_id.clone()),
            }))
            .await
            .is_err()
        {
            let _ = child.kill().await;
            return Ok(());
        }
        assistant_message_id = next_assistant_message_id;
        prompt_blocks = vec![json!({ "type": "text", "text": next_prompt })];
    }
}

pub(crate) type RequestInputFn = Box<
    dyn Fn(
            Vec<UserInputQuestion>,
        ) -> tokio::sync::oneshot::Receiver<Vec<comet_proto::UserInputAnswer>>
        + Send
        + Sync,
>;

async fn handle_session_incoming(
    incoming: Incoming,
    client: &RpcClient,
    harness: HarnessId,
    request_input: &RequestInputFn,
    events: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
) -> bool {
    match incoming {
        Incoming::Notification { method, params } => {
            if method == "session/update"
                && let Some(event) = normalize_update(&params, harness)
            {
                events.send(Ok(event)).await.ok();
            }
            true
        }
        Incoming::Request { id, method, params } if method == "session/request_permission" => {
            // The ACP child already runs in YOLO mode. Any permission request
            // that survives that policy is an explicit provider safety gate.
            handle_permission_request(id, params, client.clone(), request_input);
            true
        }
        Incoming::Request { id, method, params } if method == "elicitation/create" => {
            handle_elicitation_request(id, params, client.clone(), request_input);
            true
        }
        Incoming::Request { id, .. } => {
            client.respond_error(&id, -32601, "unsupported ACP client method");
            true
        }
        Incoming::Eof => false,
    }
}

fn handle_permission_request(
    id: Value,
    params: Value,
    client: RpcClient,
    request_input: &RequestInputFn,
) {
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let labels: Vec<String> = options
        .iter()
        .filter_map(|option| {
            option
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    let title = params
        .pointer("/toolCall/title")
        .and_then(Value::as_str)
        .unwrap_or("OMP tool");
    let receiver = request_input(vec![UserInputQuestion {
        id: "permission".into(),
        header: "Permission".into(),
        question: format!("Allow {title}?"),
        options: labels,
        multi_select: false,
    }]);
    tokio::spawn(async move {
        let selected = receiver
            .await
            .ok()
            .and_then(|answers| answers.into_iter().flat_map(|answer| answer.labels).next());
        let option_id = selected.as_deref().and_then(|label| {
            options
                .iter()
                .find(|option| option.get("name").and_then(Value::as_str) == Some(label))
                .and_then(|option| option.get("optionId"))
                .and_then(Value::as_str)
        });
        match option_id {
            Some(option_id) => client.respond(
                &id,
                json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
            ),
            None => client.respond(&id, json!({ "outcome": { "outcome": "cancelled" } })),
        }
    });
}

fn handle_elicitation_request(
    id: Value,
    params: Value,
    client: RpcClient,
    request_input: &RequestInputFn,
) {
    let questions = crate::approval::elicitation_questions(&params);
    let receiver = request_input(questions.clone());
    tokio::spawn(async move {
        let answers = receiver.await.unwrap_or_default();
        client.respond(
            &id,
            crate::approval::elicitation_response(&params, &questions, &answers),
        );
    });
}

fn content_text(content: &Value) -> Option<&str> {
    content.get("text").and_then(Value::as_str)
}

/// Tool output text from a terminal `tool_call_update`. OMP's completed
/// update carries the accumulated output as `rawOutput.content:
/// [{type:"text", text}]` (observed live; the `content` field echoes the
/// INPUT first, so `rawOutput` is the clean source). Non-conforming shapes
/// degrade to the raw JSON so an unfamiliar tool still shows something.
fn tool_output_text(update: &Value) -> Option<String> {
    let raw = update.get("rawOutput")?;
    if let Some(text) = raw.as_str() {
        return (!text.is_empty()).then(|| text.to_string());
    }
    if let Some(blocks) = raw.get("content").and_then(Value::as_array) {
        let joined = blocks
            .iter()
            .filter_map(content_text)
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    (!raw.is_null()).then(|| serde_json::to_string_pretty(raw).unwrap_or_else(|_| raw.to_string()))
}

pub(crate) fn agent_activity_status(status: &str) -> AgentActivityStatus {
    match status {
        "pending" => AgentActivityStatus::Pending,
        "completed" | "done" | "idle" => AgentActivityStatus::Completed,
        "failed" => AgentActivityStatus::Failed,
        "cancelled" | "canceled" => AgentActivityStatus::Cancelled,
        _ => AgentActivityStatus::Running,
    }
}

fn pending_agent_activities(update: &Value) -> Option<Vec<AgentActivity>> {
    let tasks = update.pointer("/rawInput/tasks")?.as_array()?;
    (!tasks.is_empty()).then(|| {
        tasks
            .iter()
            .enumerate()
            .map(|(index, task)| AgentActivity {
                id: task
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Agent {}", index + 1)),
                role: task
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or("task")
                    .to_string(),
                status: AgentActivityStatus::Pending,
                model: None,
            })
            .collect()
    })
}

fn updated_agent_activities(update: &Value) -> Option<Vec<AgentActivity>> {
    let progress = update.pointer("/rawOutput/details/progress")?.as_array()?;
    (!progress.is_empty()).then(|| {
        progress
            .iter()
            .enumerate()
            .map(|(index, activity)| AgentActivity {
                id: activity
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Agent {}", index + 1)),
                role: activity
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or("task")
                    .to_string(),
                status: agent_activity_status(
                    activity
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("running"),
                ),
                model: activity
                    .get("resolvedModel")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
            .collect()
    })
}

fn plan_items(update: &Value) -> Vec<TodoItem> {
    update
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let text = entry
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())?;
            Some(TodoItem {
                text: text.to_string(),
                done: entry.get("status").and_then(Value::as_str) == Some("completed"),
            })
        })
        .collect()
}

fn normalize_update(params: &Value, harness: HarnessId) -> Option<AgentEvent> {
    let update = params.get("update")?;
    match update.get("sessionUpdate")?.as_str()? {
        "agent_message_chunk" => Some(AgentEvent::TextDelta {
            text: content_text(update.get("content")?)?.into(),
        }),
        "agent_thought_chunk" => Some(AgentEvent::ReasoningDelta {
            text: content_text(update.get("content")?)?.into(),
        }),
        "session_info_update" => update
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(|title| AgentEvent::SessionTitleChanged {
                title: title.to_string(),
            }),
        "plan" => Some(AgentEvent::ToolCall {
            id: "omp-plan".into(),
            call: ToolCall::Todo {
                items: plan_items(update),
            },
        }),
        "tool_call" => {
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("OMP tool");
            let call = if let Some(agents) = pending_agent_activities(update) {
                ToolCall::Agent { agents }
            } else if harness == HarnessId::PrimeAgent && title == "IPython cell" {
                update
                    .pointer("/rawInput/code")
                    .and_then(Value::as_str)
                    .filter(|code| !code.trim().is_empty())
                    .map(|code| ToolCall::Exec {
                        command: code.to_string(),
                    })
                    .unwrap_or_else(|| ToolCall::Unknown {
                        name: title.into(),
                        input: update.get("rawInput").cloned(),
                    })
            } else {
                ToolCall::Unknown {
                    name: title.into(),
                    input: update.get("rawInput").cloned(),
                }
            };
            Some(AgentEvent::ToolCall {
                id: update.get("toolCallId")?.as_str()?.into(),
                call,
            })
        }
        "tool_call_update" => {
            let id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("omp-tool")
                .to_string();
            if let Some(agents) = updated_agent_activities(update) {
                Some(AgentEvent::ToolCall {
                    id,
                    call: ToolCall::Agent { agents },
                })
            } else {
                let status = update.get("status").and_then(Value::as_str).unwrap_or("");
                matches!(status, "completed" | "failed").then(|| AgentEvent::ToolResult {
                    id,
                    is_error: status == "failed",
                    output: tool_output_text(update).map(comet_proto::truncate_tool_output),
                })
            }
        }
        "usage_update" => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsof_probe_ignores_readers_and_requires_access_fields() {
        let output = b"p10\nf3\nar\nnsession.jsonl\np11\nf4\naw\nnsession.jsonl\np12\nf5\nau\nnsession.jsonl\n";
        assert_eq!(parse_lsof_writer_pids(output), Some(vec![11, 12]));
        assert_eq!(
            parse_lsof_writer_pids(b"p10\nf3\nar\nnsession.jsonl\n"),
            Some(Vec::new())
        );
        assert_eq!(
            parse_lsof_writer_pids(b"p10\nf3\na \nnsession.jsonl\n"),
            None
        );
        assert_eq!(parse_lsof_writer_pids(b"p10\nf3\nnsession.jsonl\n"), None);
    }

    #[test]
    fn linux_fdinfo_and_proc_stat_classify_ownership() {
        assert_eq!(
            fdinfo_text_is_write_capable("flags:\t0100000\n"),
            Some(false)
        );
        assert_eq!(
            fdinfo_text_is_write_capable("flags:\t0100001\n"),
            Some(true)
        );
        assert_eq!(
            fdinfo_text_is_write_capable("flags:\t0100002\n"),
            Some(true)
        );
        assert_eq!(fdinfo_text_is_write_capable("pos:\t0\n"), None);
        assert_eq!(
            linux_parent_and_group("321 (worker name) S 42 77 77 0 -1 0"),
            Some((42, 77))
        );
    }

    #[test]
    fn supervised_run_wraps_only_the_persistent_rpc_process() {
        let command = OmpHarness::new()
            .with_supervisor_executable("/opt/comet/bin/comet")
            .run_command(Path::new("/opt/omp/bin/omp"), &run_request(None));
        assert_eq!(command.as_std().get_program(), "/opt/comet/bin/comet");
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args[0], OMP_SUPERVISOR_MARKER);
        assert_eq!(args[1], std::process::id().to_string());
        assert_eq!(args[2], "/opt/omp/bin/omp");
        assert_eq!(&args[3..7], ["--mode", "rpc", "--approval-mode", "yolo"]);

        let catalog = OmpHarness::new()
            .with_supervisor_executable("/opt/comet/bin/comet")
            .command_catalog_command(Path::new("/opt/omp/bin/omp"), "/tmp");
        assert_eq!(catalog.as_std().get_program(), "/opt/omp/bin/omp");
    }
    fn configured_env(command: &Command, key: &str) -> Option<String> {
        command
            .as_std()
            .get_envs()
            .find(|(name, _)| name.to_string_lossy() == key)
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
    }

    #[test]
    fn resumed_acp_runs_use_distinct_assistant_message_ids() {
        let session_id = "same-durable-session";
        let first_run = uuid::Uuid::from_u128(1);
        let resumed_run = uuid::Uuid::from_u128(2);

        assert_ne!(
            acp_assistant_message_id(session_id, &first_run, 0),
            acp_assistant_message_id(session_id, &resumed_run, 0)
        );
    }

    #[test]
    fn run_config_preserves_inherited_overlays_and_wins_last() {
        let inherited =
            std::env::join_paths([Path::new("/tmp/user-a.yml"), Path::new("/tmp/user-b.yml")])
                .unwrap();
        let combined =
            config_overlay_paths(Some(&inherited), Path::new("/tmp/comet-retry.yml")).unwrap();
        assert_eq!(
            std::env::split_paths(&combined).collect::<Vec<_>>(),
            [
                PathBuf::from("/tmp/user-a.yml"),
                PathBuf::from("/tmp/user-b.yml"),
                PathBuf::from("/tmp/comet-retry.yml"),
            ]
        );
    }

    #[test]
    fn run_config_bounds_retries_and_computer_screenshots() {
        let path;
        {
            let config = OmpRunConfig::create().unwrap();
            path = config.path.clone();
            assert_eq!(std::fs::read(&path).unwrap(), OMP_RUN_CONFIG);
        }
        assert!(!path.exists(), "temporary OMP overlay must be removed");
    }

    #[test]
    fn local_commands_are_marked_while_scaffold_commands_stay_ci() {
        let local =
            OmpHarness::new().rpc_mode_command(Path::new("/usr/local/bin/omp"), "/workspace", true);
        let scaffold = OmpHarness::scaffold_host().rpc_mode_command(
            Path::new("/usr/local/bin/omp"),
            "/workspace",
            true,
        );

        assert_eq!(configured_env(&local, "CI").as_deref(), Some("false"));
        assert_eq!(
            configured_env(&local, LOCAL_RUNTIME_ENV).as_deref(),
            Some("1")
        );
        assert_eq!(configured_env(&scaffold, "CI").as_deref(), Some("true"));
        assert_eq!(
            configured_env(&scaffold, LOCAL_RUNTIME_ENV),
            None,
            "Scaffold must not mark commands as local"
        );
    }

    #[test]
    fn scaffold_command_invokes_only_omp_rpc_mode_with_hardening() {
        let harness = OmpHarness::scaffold_host()
            .with_auth_broker_environment(AuthBrokerEnvironment::default());
        let command = harness.rpc_mode_command(Path::new("/usr/local/bin/omp"), "/workspace", true);
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.first().map(String::as_str), Some("--mode"));
        assert_eq!(args.get(1).map(String::as_str), Some("rpc"));
        for required in [
            "--approval-mode",
            "yolo",
            "--profile",
            SCAFFOLD_PROFILE,
            "--no-extensions",
            "--no-skills",
            "--no-rules",
        ] {
            assert!(args.iter().any(|argument| argument == required));
        }
        assert!(
            !args.iter().any(|argument| argument == "claude"
                || argument == "codex"
                || argument == "opencode")
        );
    }

    fn run_request(resume: Option<&str>) -> RunRequest {
        RunRequest {
            prompt: "test".into(),
            model: None,
            agent_account_id: None,
            reasoning: None,
            model_options: serde_json::Map::new(),
            cwd: "/workspace".into(),
            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            resume: resume.map(str::to_string),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn run_command_stays_yolo_and_threads_run_flags() {
        let request = RunRequest {
            model: Some("anthropic/claude-haiku-4-5".into()),
            reasoning: Some(ReasoningLevel::XHigh),
            ..run_request(Some("session-1"))
        };
        let command = OmpHarness::new().run_command(Path::new("/usr/local/bin/omp"), &request);
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--mode",
                "rpc",
                "--approval-mode",
                "yolo",
                "--model",
                "anthropic/claude-haiku-4-5",
                "--thinking",
                "xhigh",
                "--resume",
                "session-1",
            ]
        );
    }

    #[test]
    fn run_command_omits_default_model_and_absent_flags() {
        let command =
            OmpHarness::new().run_command(Path::new("/usr/local/bin/omp"), &run_request(None));
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--mode", "rpc", "--approval-mode", "yolo"]);
    }

    #[test]
    fn shared_inference_uses_an_explicit_gateway_extension_and_provider() {
        let temp = tempfile::tempdir().unwrap();
        let mut request = run_request(None);
        request.model = Some("gpt-5.6-sol".into());
        let mut command =
            OmpHarness::scaffold_host().run_command(Path::new("/usr/local/bin/omp"), &request);
        configure_inference_gateway(
            &mut command,
            temp.path(),
            &InferenceRoute {
                base_url: "http://127.0.0.1:41234".into(),
                token: "local-inference-token".into(),
                provider: "openai".into(),
                model: "gpt-5.6-sol".into(),
            },
        )
        .unwrap();
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        let extension = temp.path().join("comet-runtime/agent-auth-gateway.ts");
        assert!(extension.is_file());
        assert!(args.windows(2).any(|pair| {
            pair == [
                "--extension".to_string(),
                extension.to_string_lossy().into_owned(),
            ]
        }));
        assert_eq!(
            &args[args.len() - 2..],
            ["--model", "comet-openai/gpt-5.6-sol"]
        );
    }

    #[test]
    fn scaffold_inference_profile_pins_the_scoped_model() {
        let runtime_dir = tempfile::tempdir().unwrap();
        let profile_dir = runtime_dir.path().join("omp-inference");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(
            profile_dir.join("profile.json"),
            r#"{"profile":"scaffold-host","model":"scaffold-openai/gpt-5.6-sol"}"#,
        )
        .unwrap();

        let model =
            scaffold_inference_model_at(runtime_dir.path(), Some("openai-codex/gpt-5.6-sol"))
                .unwrap();

        assert_eq!(model, "scaffold-openai/gpt-5.6-sol");
        assert!(
            scaffold_inference_model_at(runtime_dir.path(), Some("openai-codex/gpt-5.6-terra"),)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );

        std::fs::write(
            profile_dir.join("profile.json"),
            r#"{"profile":"scaffold-host","model":"scaffold-anthropic/claude-opus-4-1"}"#,
        )
        .unwrap();
        assert_eq!(
            scaffold_inference_model_at(runtime_dir.path(), Some("anthropic/claude-opus-4-1"),)
                .unwrap(),
            "scaffold-anthropic/claude-opus-4-1"
        );
        assert!(
            scaffold_inference_model_at(runtime_dir.path(), Some("openai-codex/claude-opus-4-1"),)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
    }

    #[test]
    fn broker_configuration_is_environment_only_and_scaffold_stays_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let token_file = temp.path().join("broker.token");
        std::fs::write(&token_file, "super-secret-token\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let harness =
            OmpHarness::scaffold_host().with_auth_broker_environment(AuthBrokerEnvironment {
                url: Some("https://broker.example.test".into()),
                token: Some("workstation-token-must-not-leak".into()),
                token_file: Some(token_file.clone().into_os_string()),
            });
        let catalog_command =
            harness.command_catalog_command(Path::new("/usr/local/bin/omp"), "/workspace");
        let catalog_process = catalog_command.as_std();
        assert!(catalog_process.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(AUTH_BROKER_TOKEN_ENV) && value.is_none()
        }));
        assert!(catalog_process.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(AUTH_BROKER_TOKEN_FILE_ENV) && value.is_none()
        }));
        assert!(
            token_file.exists(),
            "command discovery must preserve the token for the persistent RPC child"
        );

        let command = harness.rpc_mode_command(Path::new("/usr/local/bin/omp"), "/workspace", true);
        let process = command.as_std();
        let args: Vec<_> = process
            .get_args()
            .map(|value| value.to_string_lossy())
            .collect();
        assert_eq!(
            args,
            [
                "--mode",
                "rpc",
                "--approval-mode",
                "yolo",
                "--profile",
                SCAFFOLD_PROFILE,
                "--no-extensions",
                "--no-skills",
                "--no-rules"
            ]
        );
        assert!(
            !args
                .iter()
                .any(|value| value.contains("super-secret-token"))
        );
        let environment: std::collections::BTreeMap<_, _> = process
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
            .collect();
        assert_eq!(
            environment.get(std::ffi::OsStr::new(AUTH_BROKER_URL_ENV)),
            Some(&std::ffi::OsString::from("https://broker.example.test"))
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new(AUTH_BROKER_TOKEN_ENV)),
            Some(&std::ffi::OsString::from("super-secret-token"))
        );
        assert!(!token_file.exists(), "broker token file must be single use");
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
        ] {
            assert!(
                process
                    .get_envs()
                    .any(|(key, value)| { key == std::ffi::OsStr::new(name) && value.is_none() })
            );
        }

        let insecure_file = temp.path().join("insecure-broker.token");
        std::fs::write(&insecure_file, "must-not-propagate\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&insecure_file, std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        let insecure = OmpHarness::scaffold_host()
            .with_auth_broker_environment(AuthBrokerEnvironment {
                url: Some("https://broker.example.test".into()),
                token: Some("workstation-token-must-not-leak".into()),
                token_file: Some(insecure_file.clone().into_os_string()),
            })
            .rpc_mode_command(Path::new("/usr/local/bin/omp"), "/workspace", true);
        assert!(insecure.as_std().get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(AUTH_BROKER_TOKEN_ENV) && value.is_none()
        }));
        assert!(
            !insecure_file.exists(),
            "rejected token files must still be removed"
        );
    }

    #[test]
    fn parses_advisor_config_from_omp_settings() {
        let config = parse_advisor_config(&json!({
            "advisor.enabled": {"value": true},
            "advisor.subagents": {"value": true},
            "advisor.syncBacklog": {"value": "5"},
            "advisor.immuneTurns": {"value": 8},
            "modelRoles": {"value": {
                "advisor": "anthropic/claude-opus-5:xhigh",
                "task": "openai-codex/gpt-5.6-sol"
            }}
        }))
        .unwrap();

        assert!(config.enabled);
        assert_eq!(config.model, "anthropic/claude-opus-5:xhigh");
        assert!(config.subagents);
        assert_eq!(config.sync_backlog, OmpAdvisorSyncBacklog::Five);
        assert_eq!(config.immune_turns, 8);
    }

    #[test]
    fn rejects_advisor_backlog_values_comet_cannot_render() {
        let error = parse_advisor_config(&json!({
            "advisor.syncBacklog": {"value": "2"}
        }))
        .unwrap_err();
        assert!(error.to_string().contains("advisor.syncBacklog"));
    }

    #[test]
    fn normalizes_text_reasoning_and_tool_updates() {
        assert_eq!(
            normalize_update(
                &json!({"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}),
                HarnessId::Omp,
            ),
            Some(AgentEvent::TextDelta { text: "hi".into() })
        );
        assert_eq!(
            normalize_update(
                &json!({"update":{"sessionUpdate":"session_info_update","title":"Probe title"}}),
                HarnessId::Omp,
            ),
            Some(AgentEvent::SessionTitleChanged {
                title: "Probe title".into()
            })
        );
        assert_eq!(
            normalize_update(
                &json!({"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"why"}}}),
                HarnessId::Omp,
            ),
            Some(AgentEvent::ReasoningDelta { text: "why".into() })
        );
        assert_eq!(
            normalize_update(
                &json!({"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"failed"}}),
                HarnessId::Omp,
            ),
            Some(AgentEvent::ToolResult {
                id: "t1".into(),
                is_error: true,
                output: None,
            })
        );
        // The observed completed-update shape: output rides `rawOutput.content`
        // as text blocks (`content` echoes the input first — never used).
        assert_eq!(
            normalize_update(
                &json!({"update":{
                    "sessionUpdate":"tool_call_update","toolCallId":"t2","status":"completed",
                    "rawOutput": {"content":[{"type":"text","text":"hello\n/tmp"}], "details":{}},
                    "content": [{"type":"content","content":{"type":"text","text":"$ echo hello"}}]
                }}),
                HarnessId::Omp,
            ),
            Some(AgentEvent::ToolResult {
                id: "t2".into(),
                is_error: false,
                output: Some("hello\n/tmp".into()),
            })
        );
    }

    #[test]
    fn normalizes_omp_plan_updates_as_persistent_todos() {
        let update = json!({
            "update": {
                "sessionUpdate": "plan",
                "entries": [
                    {"content": "Inspect the failure", "priority": "medium", "status": "completed"},
                    {"content": "Ship the fix", "priority": "high", "status": "in_progress"},
                    {"content": "   ", "status": "pending"}
                ]
            }
        });

        assert_eq!(
            normalize_update(&update, HarnessId::Omp),
            Some(AgentEvent::ToolCall {
                id: "omp-plan".into(),
                call: ToolCall::Todo {
                    items: vec![
                        TodoItem {
                            text: "Inspect the failure".into(),
                            done: true,
                        },
                        TodoItem {
                            text: "Ship the fix".into(),
                            done: false,
                        },
                    ],
                },
            })
        );
    }

    #[test]
    fn normalizes_task_subagent_lifecycle() {
        let initial = json!({
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "task-1",
                "title": "Launching one read-only scout",
                "rawInput": {
                    "tasks": [{"agent": "scout", "task": "Inspect the target"}]
                }
            }
        });
        assert_eq!(
            normalize_update(&initial, HarnessId::Omp),
            Some(AgentEvent::ToolCall {
                id: "task-1".into(),
                call: ToolCall::Agent {
                    agents: vec![AgentActivity {
                        id: "Agent 1".into(),
                        role: "scout".into(),
                        status: AgentActivityStatus::Pending,
                        model: None,
                    }],
                },
            })
        );

        let running = json!({
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "task-1",
                "status": "in_progress",
                "rawOutput": {
                    "details": {
                        "progress": [{
                            "id": "PleasantBeetle",
                            "agent": "scout",
                            "status": "running",
                            "resolvedModel": "anthropic/claude-opus-5:xhigh"
                        }]
                    }
                }
            }
        });
        assert_eq!(
            normalize_update(&running, HarnessId::Omp),
            Some(AgentEvent::ToolCall {
                id: "task-1".into(),
                call: ToolCall::Agent {
                    agents: vec![AgentActivity {
                        id: "PleasantBeetle".into(),
                        role: "scout".into(),
                        status: AgentActivityStatus::Running,
                        model: Some("anthropic/claude-opus-5:xhigh".into()),
                    }],
                },
            })
        );
    }

    #[test]
    fn prime_ipython_cells_render_as_their_code() {
        let update = json!({
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "python-1",
                "title": "IPython cell",
                "kind": "execute",
                "rawInput": {"code": "print(\"PRIME_RENDER_MARKER\")"}
            }
        });

        assert_eq!(
            normalize_update(&update, HarnessId::PrimeAgent),
            Some(AgentEvent::ToolCall {
                id: "python-1".into(),
                call: ToolCall::Exec {
                    command: "print(\"PRIME_RENDER_MARKER\")".into(),
                },
            })
        );
        assert_eq!(
            normalize_update(&update, HarnessId::Omp),
            Some(AgentEvent::ToolCall {
                id: "python-1".into(),
                call: ToolCall::Unknown {
                    name: "IPython cell".into(),
                    input: Some(json!({"code": "print(\"PRIME_RENDER_MARKER\")"})),
                },
            })
        );
    }

    #[test]
    fn catalog_descriptions_do_not_claim_verified_availability() {
        let models = models_from_catalog(
            br#"{"models":[{"selector":"openai-codex/gpt-5.6-sol","name":"GPT-5.6 Sol","contextWindow":1000,"maxTokens":100,"thinking":[]}]}"#,
        )
        .unwrap();

        for model in models {
            assert!(
                model
                    .description
                    .as_deref()
                    .unwrap()
                    .contains("does not verify run availability")
                    || model
                        .description
                        .as_deref()
                        .unwrap()
                        .contains("run availability is not verified")
            );
            assert!(
                model
                    .description
                    .as_deref()
                    .unwrap()
                    .contains("authorization")
            );
        }
    }

    #[test]
    fn catalog_keeps_every_model_selector() {
        let models = models_from_catalog(
            br#"{"models":[
                {"selector":"openai-codex/gpt-5.6-luna","name":"GPT-5.6 Luna","contextWindow":1000,"maxTokens":100,"thinking":[]},
                {"selector":"openai/gpt-5.4","name":"GPT-5.4","contextWindow":1000,"maxTokens":100,"thinking":["low","high"]},
                {"selector":"openrouter/deepseek/deepseek-v4-pro","name":"DeepSeek V4 Pro","contextWindow":1000,"maxTokens":100,"thinking":["high"]},
                {"selector":"openrouter/tngtech/deepseek-r1t2-chimera","name":"DeepSeek R1T2 Chimera","contextWindow":1000,"maxTokens":100,"thinking":["high"]},
                {"selector":"anthropic/claude-fable-5","name":"Fable 5","contextWindow":1000,"maxTokens":100,"thinking":[]}
            ]}"#,
        )
        .unwrap();

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "openai-codex/gpt-5.6-luna",
                "openai/gpt-5.4",
                "openrouter/deepseek/deepseek-v4-pro",
                "openrouter/tngtech/deepseek-r1t2-chimera",
                "anthropic/claude-fable-5",
            ]
        );
    }
    #[test]
    fn catalog_keeps_models_with_incomplete_size_metadata() {
        let models = models_from_catalog(
            br#"{"models":[
                {"selector":"openrouter/meta/muse-glimmer-30b","name":"Muse Glimmer","contextWindow":131072,"maxTokens":null,"thinking":[]},
                {"selector":"openrouter/deepseek/incomplete","name":"Incomplete DeepSeek","contextWindow":null,"maxTokens":100,"thinking":[]},
                {"selector":"openrouter/unknown","name":"Unknown","contextWindow":null,"maxTokens":null,"thinking":[]}
            ]}"#,
        )
        .unwrap();

        assert_eq!(models.len(), 3);
        assert!(
            models[0]
                .description
                .as_deref()
                .unwrap()
                .starts_with("131072 context")
        );
        assert!(
            models[1]
                .description
                .as_deref()
                .unwrap()
                .starts_with("100 max output")
        );
        assert!(
            models[2]
                .description
                .as_deref()
                .unwrap()
                .starts_with("Listed in OMP")
        );
    }

    #[test]
    fn session_matcher_does_not_spend_directory_budget_on_journals() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let cwd_dir = root.join("workspace");
        std::fs::create_dir_all(&cwd_dir).unwrap();
        for index in 0..4 {
            std::fs::write(root.join(format!("unrelated-{index}.jsonl")), "{}\n").unwrap();
        }
        let session_id = "019fc051-d259-7000-992c-40d61e37b213";
        let expected = cwd_dir.join(format!("timestamp_{session_id}.jsonl"));
        std::fs::write(
            &expected,
            format!(r#"{{"type":"session","id":"{session_id}"}}"#),
        )
        .unwrap();

        let (matches, exhaustive) =
            matching_session_files_with_limit(std::slice::from_ref(&root), session_id, 2);

        assert!(exhaustive);
        assert_eq!(matches, vec![expected]);
    }

    #[test]
    fn timeout_messages_name_the_blocking_stage() {
        assert_eq!(
            stage_timeout_message("OMP ACP", "initialize", Duration::from_secs(7)),
            "OMP ACP initialize timed out after 7s"
        );
    }
}
