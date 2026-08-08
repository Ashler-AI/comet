//! OMP harness adapter over Agent Client Protocol (ACP) stdio.
//!
//! Comet owns execution through `omp acp`; OMP's read-only `models --json`
//! command supplies the selectable provider/model catalog.
//! One ACP child and session stay alive across turn-boundary steering until
//! Comet closes the steering mailbox or interrupts the run.

pub(crate) mod rpc;

use std::collections::VecDeque;
use std::future::Future;
use std::io::{BufRead as _, BufReader, Read as _};
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

use crate::{Harness, HarnessError, RunControls};
use rpc::{Incoming, RpcClient};

const ACP_PROTOCOL_VERSION: i64 = 1;
const ACP_INITIALIZE_DEADLINE: Duration = Duration::from_secs(15);
const ACP_SESSION_DEADLINE: Duration = Duration::from_secs(30);
const ACP_CONFIGURE_DEADLINE: Duration = Duration::from_secs(15);
const ACP_PROMPT_INACTIVITY_LOG_INTERVAL: Duration = Duration::from_secs(300);
const SCAFFOLD_PROFILE: &str = "scaffold-host";
const AUTH_BROKER_URL_ENV: &str = "OMP_AUTH_BROKER_URL";
const AUTH_BROKER_TOKEN_ENV: &str = "OMP_AUTH_BROKER_TOKEN";
const AUTH_BROKER_TOKEN_FILE_ENV: &str = "OMP_AUTH_BROKER_TOKEN_FILE";

const OMP_SESSION_DIRECTORY_SCAN_LIMIT: usize = 10_000;
const OMP_SESSION_HEADER_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisorConfigUpdate {
    Enabled(bool),
    Model(String),
    Subagents(bool),
    SyncBacklog(OmpAdvisorSyncBacklog),
    ImmuneTurns(u32),
}
/// Whether another process currently owns an OMP session journal.
///
/// OMP 17.2.9 has no persisted local attach endpoint or writer lock. Its
/// session manager keeps an append descriptor open between writes for an active
/// persisted session. We use an exact-path descriptor probe and fail closed
/// when the platform cannot answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionWriterState {
    Active,
    Inactive,
    Unknown,
}

fn session_writer_state_with(path: &Path, executable: &Path) -> Option<SessionWriterState> {
    let output = ProcessCommand::new(executable)
        .args(["-F", "p", "--"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if output.stdout.split(|byte| *byte == b'\n').any(|line| {
        line.strip_prefix(b"p")
            .is_some_and(|pid| !pid.is_empty() && pid.iter().all(u8::is_ascii_digit))
    }) {
        return Some(SessionWriterState::Active);
    }
    if output.status.code() == Some(1) && output.stderr.is_empty() {
        return Some(SessionWriterState::Inactive);
    }
    Some(SessionWriterState::Unknown)
}

#[cfg(target_os = "linux")]
fn linux_session_writer_state(path: &Path) -> SessionWriterState {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(target) = std::fs::metadata(path) else {
        return SessionWriterState::Unknown;
    };
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return SessionWriterState::Unknown;
    };
    // SAFETY: geteuid has no preconditions and does not retain pointers.
    let effective_uid = unsafe { libc::geteuid() };
    let mut incomplete = false;
    for process in processes {
        let Ok(process) = process else {
            incomplete = true;
            continue;
        };
        if process
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
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
            match std::fs::metadata(descriptor.path()) {
                Ok(metadata)
                    if metadata.dev() == target.dev() && metadata.ino() == target.ino() =>
                {
                    return SessionWriterState::Active;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => incomplete = true,
            }
        }
    }
    if incomplete {
        SessionWriterState::Unknown
    } else {
        SessionWriterState::Inactive
    }
}

/// Probe one exact OMP JSONL path for a live writer.
///
/// Linux uses `/proc/<pid>/fd`, so Scaffold images do not need an `lsof`
/// package. macOS and other Unix hosts resolve the native absolute `lsof`
/// locations before consulting `PATH`. A missing tool or failed/incomplete
/// probe is `Unknown`, never `Inactive`.
pub fn session_writer_state(path: &Path) -> SessionWriterState {
    #[cfg(target_os = "linux")]
    match linux_session_writer_state(path) {
        SessionWriterState::Active => return SessionWriterState::Active,
        SessionWriterState::Inactive => return SessionWriterState::Inactive,
        SessionWriterState::Unknown => {}
    }

    for executable in [
        Path::new("/usr/sbin/lsof"),
        Path::new("/usr/bin/lsof"),
        Path::new("lsof"),
    ] {
        if let Some(state) = session_writer_state_with(path, executable) {
            return state;
        }
    }
    SessionWriterState::Unknown
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

fn propagate_auth_broker_environment(
    command: &mut Command,
    scaffold_host: bool,
    include_token: bool,
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
    if let Some(url) = std::env::var_os(AUTH_BROKER_URL_ENV).filter(|value| !value.is_empty()) {
        command.env(AUTH_BROKER_URL_ENV, url);
    }
    if !include_token {
        return;
    }
    // Local controllers may inherit a workstation broker token. A scoped host
    // must never accept that long-lived credential and can use only its
    // single-use token file projection.
    if !scaffold_host
        && let Some(token) =
            std::env::var_os(AUTH_BROKER_TOKEN_ENV).filter(|value| !value.is_empty())
    {
        command.env(AUTH_BROKER_TOKEN_ENV, token);
        return;
    }
    let Some(path) = std::env::var_os(AUTH_BROKER_TOKEN_FILE_ENV).filter(|value| !value.is_empty())
    else {
        return;
    };
    // OMP 17.2.9 consumes OMP_AUTH_BROKER_TOKEN. Comet accepts a mode-0600,
    // single-use file so supervisors never place the bearer in argv or their
    // long-lived environment. Remove it before parsing or spawning on every
    // outcome, including malformed content and permission failures.
    let metadata = std::fs::symlink_metadata(&path);
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
        std::fs::read(&path).ok()
    });
    let _ = std::fs::remove_file(&path);
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
    scaffold_host: bool,
    interrupt_grace: Duration,
    session_dirs: Option<Vec<PathBuf>>,
    session_writer_probe: Option<PathBuf>,
}

impl Default for OmpHarness {
    fn default() -> Self {
        Self {
            executable: None,
            scaffold_host: false,
            interrupt_grace: Duration::from_secs(3),
            session_dirs: None,
            session_writer_probe: None,
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
                    return Err(HarnessError::Protocol(
                        "This OMP session is already running. Close it before resuming here."
                            .into(),
                    ));
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
    ) -> Command {
        let mut command = Command::new(executable);
        crate::compose_child_path(&mut command, executable);
        propagate_auth_broker_environment(
            &mut command,
            self.scaffold_host,
            include_auth_broker_token,
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

    fn acp_command(
        &self,
        executable: &Path,
        cwd: &str,
        include_auth_broker_token: bool,
    ) -> Command {
        let mut command = self.base_command(executable, cwd, include_auth_broker_token);
        command.args(["acp", "--approval-mode", "yolo"]);
        if self.scaffold_host {
            command.args([
                "--profile",
                SCAFFOLD_PROFILE,
                "--no-extensions",
                "--no-skills",
                "--no-rules",
            ]);
        }
        command
    }

    fn command(&self, executable: &Path, cwd: &str, _auto_approve: bool) -> Command {
        self.acp_command(executable, cwd, true)
    }
    fn command_catalog_command(&self, executable: &Path, cwd: &str) -> Command {
        let mut command = self.acp_command(executable, cwd, false);
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
    context_window: u64,
    max_tokens: u64,
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
        .filter(|model| crate::is_curated_comet_model(&model.selector))
        .map(|model| Model {
            id: model.selector,
            label: model.name,
            description: Some(format!(
                "{} context · {} max output · Listed in OMP's catalog; run availability is not verified; authorization is not verified",
                model.context_window, model.max_tokens
            )),
            reasoning_levels: model
                .thinking
                .unwrap_or_default()
                .into_iter()
                .filter_map(|level| serde_json::from_value(Value::String(level)).ok())
                .collect(),
            options: Vec::new(),
        })
        .collect())
}

fn commands_from_update(params: &Value) -> Option<Vec<HarnessCommand>> {
    let update = params.get("update")?;
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("available_commands_update") {
        return None;
    }
    Some(
        update
            .get("availableCommands")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|command| {
                Some(HarnessCommand {
                    name: command.get("name")?.as_str()?.to_string(),
                    description: command
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input_hint: command
                        .pointer("/input/hint")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
            })
            .collect(),
    )
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
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let executable = self.resolve_executable()?;
        let output = self
            .base_command(&executable, "", false)
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
            .ok_or_else(|| HarnessError::Protocol("OMP ACP child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("OMP ACP child has no stdout".into()))?;
        let (client, mut incoming) = RpcClient::new(stdin, stdout);
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            client
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": ACP_PROTOCOL_VERSION,
                        "clientCapabilities": {
                            "elicitation": { "form": {}, "url": {} }
                        },
                        "clientInfo": {
                            "name": "ashler-comet",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }),
                )
                .await?;
            client
                .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
                .await?;
            loop {
                match incoming.recv().await {
                    Some(Incoming::Notification { method, params })
                        if method == "session/update" =>
                    {
                        if let Some(commands) = commands_from_update(&params) {
                            return Ok(commands);
                        }
                    }
                    Some(Incoming::Request { id, .. }) => {
                        client.respond_error(&id, -32601, "unsupported ACP client method");
                    }
                    Some(Incoming::Eof) | None => {
                        return Err(HarnessError::Protocol(
                            "OMP ACP command catalog closed before advertising commands".into(),
                        ));
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| HarnessError::Protocol("OMP command catalog timed out".into()))?;
        let _ = child.kill().await;
        result
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let executable = self.resolve_executable()?;
        self.ensure_resume_has_no_writer(request.resume.as_deref())?;
        let mut child = self
            .command(&executable, &request.cwd, request.auto_approve)
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
            .ok_or_else(|| HarnessError::Protocol("OMP ACP child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("OMP ACP child has no stdout".into()))?;
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
        let (client, incoming) = RpcClient::new(stdin, stdout);
        let (events, receiver) = mpsc::channel(256);
        let interrupt_grace = self.interrupt_grace;
        tokio::spawn(async move {
            if let Err(error) = run_acp(
                AcpProcess::new(child, client, incoming, stderr_tail),
                request,
                controls,
                events.clone(),
                interrupt_grace,
                AcpRunOptions {
                    harness: HarnessId::Omp,
                    process_label: "OMP ACP",
                    preloaded_session_id: None,
                    reported_session_dir: None,
                    configure_session: true,
                    persistent: true,
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
            "{} protocol version mismatch",
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
    let mut assistant_message_id = format!("acp-{session_id}");
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
    } = controls;
    let mut queued_prompts = VecDeque::new();
    let mut steering_open = true;
    let mut prompt_blocks = first_prompt;
    let mut turn_number = 0_u64;
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
                response = &mut prompt => match response {
                    Ok(response) => break response,
                    Err(error) => {
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
            return Err(HarnessError::Protocol(crate::crash_message(
                options.process_label,
                child.try_wait().ok().flatten(),
                &stderr_tail,
            )));
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
                },
                _ = events.closed() => {
                    let _ = child.kill().await;
                    return Ok(());
                }
            }
        };

        turn_number += 1;
        let next_assistant_message_id = format!("acp-{session_id}-{turn_number}");
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

type RequestInputFn = Box<
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

fn agent_activity_status(status: &str) -> AgentActivityStatus {
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
    fn scaffold_command_invokes_only_omp_acp_with_hardening() {
        let harness = OmpHarness::scaffold_host();
        let command = harness.command(Path::new("/usr/local/bin/omp"), "/workspace", true);
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.first().map(String::as_str), Some("acp"));
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

    #[test]
    fn command_keeps_yolo_for_a_legacy_approval_opt_out() {
        let command =
            OmpHarness::new().command(Path::new("/usr/local/bin/omp"), "/workspace", false);
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["acp", "--approval-mode", "yolo"]);
    }

    #[test]
    fn broker_configuration_is_environment_only_and_scaffold_stays_isolated() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = tempfile::tempdir().unwrap();
        let token_file = temp.path().join("broker.token");
        std::fs::write(&token_file, "super-secret-token\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        unsafe {
            std::env::set_var(AUTH_BROKER_URL_ENV, "https://broker.example.test");
            std::env::set_var(AUTH_BROKER_TOKEN_ENV, "workstation-token-must-not-leak");
            std::env::set_var(AUTH_BROKER_TOKEN_FILE_ENV, &token_file);
        }
        let catalog_command = OmpHarness::scaffold_host()
            .command_catalog_command(Path::new("/usr/local/bin/omp"), "/workspace");
        let catalog_process = catalog_command.as_std();
        assert!(catalog_process.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(AUTH_BROKER_TOKEN_ENV) && value.is_none()
        }));
        assert!(catalog_process.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(AUTH_BROKER_TOKEN_FILE_ENV) && value.is_none()
        }));
        assert!(
            token_file.exists(),
            "command discovery must preserve the token for the persistent ACP child"
        );

        let command = OmpHarness::scaffold_host().command(
            Path::new("/usr/local/bin/omp"),
            "/workspace",
            true,
        );
        let process = command.as_std();
        let args: Vec<_> = process
            .get_args()
            .map(|value| value.to_string_lossy())
            .collect();
        assert_eq!(
            args,
            [
                "acp",
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
        unsafe { std::env::set_var(AUTH_BROKER_TOKEN_FILE_ENV, &insecure_file) };
        let insecure = OmpHarness::scaffold_host().command(
            Path::new("/usr/local/bin/omp"),
            "/workspace",
            true,
        );
        assert!(insecure.as_std().get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(AUTH_BROKER_TOKEN_ENV) && value.is_none()
        }));
        assert!(
            !insecure_file.exists(),
            "rejected token files must still be removed"
        );
        unsafe {
            std::env::remove_var(AUTH_BROKER_URL_ENV);
            std::env::remove_var(AUTH_BROKER_TOKEN_ENV);
            std::env::remove_var(AUTH_BROKER_TOKEN_FILE_ENV);
        }
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
                is_error: true
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
    fn catalog_keeps_only_curated_models_plus_default() {
        let models = models_from_catalog(
            br#"{"models":[
                {"selector":"openai-codex/gpt-5.6-luna","name":"GPT-5.6 Luna","contextWindow":1000,"maxTokens":100,"thinking":[]},
                {"selector":"prime-inference/openai/gpt-5.6-terra-pro","name":"GPT-5.6 Terra Pro","contextWindow":1000,"maxTokens":100,"thinking":[]},
                {"selector":"openai-codex/gpt-5.5","name":"GPT-5.5","contextWindow":1000,"maxTokens":100,"thinking":[]},
                {"selector":"anthropic/claude-fable-5","name":"Fable 5","contextWindow":1000,"maxTokens":100,"thinking":[]},
                {"selector":"prime-inference/anthropic/claude-sonnet-5","name":"Claude Sonnet 5","contextWindow":1000,"maxTokens":100,"thinking":[]},
                {"selector":"prime-inference/moonshotai/kimi-k3","name":"Kimi K3","contextWindow":1000,"maxTokens":100,"thinking":[]},
                {"selector":"prime-inference/x-ai/grok-4.20-multi-agent","name":"Grok 4.20 Multi-Agent","contextWindow":1000,"maxTokens":100,"thinking":[]},
                {"selector":"prime-inference/z-ai/glm-5.2","name":"GLM 5.2","contextWindow":1000,"maxTokens":100,"thinking":[]}
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
                "prime-inference/openai/gpt-5.6-terra-pro",
                "anthropic/claude-fable-5",
                "prime-inference/anthropic/claude-sonnet-5",
                "prime-inference/moonshotai/kimi-k3",
                "prime-inference/x-ai/grok-4.20-multi-agent",
            ]
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
