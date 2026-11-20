//! Crew: OMP (oh-my-pi), Crew's build of the Pi coding agent, driven over its
//! RPC mode (docs/crew/zgx-thin-port.md, "OMP"). Ported from Crew's
//! `crates/harness/src/omp/` (Ashler-AI/comet), trimmed to what a managed turn
//! needs. Upstream's Pi driver can't drive it: OMP's flags, commands and turn
//! end differ.
//!
//! - Models are `omp models --json --no-extensions`, one per row as
//!   `{provider}/{id}`, listed as a managed launch would run
//!   ([`crate::crew_inference::managed_probe`]) and cached as upstream's Pi
//!   catalog is.
//! - A managed run (a route in [`RunRequest::env`]) reaches the engine's relay
//!   and nothing else: `auth_gateway.ts` registers `comet-anthropic` and
//!   `comet-openai` from the relay's `GET /v1/models`, the run's model and
//!   every model role are `comet-{provider}/{id}`, and the child never gets
//!   `COMET_INFERENCE_TOKEN`. Unmanaged, OMP runs `{provider}/{id}` on its
//!   own configuration.
//! - A resume opens the exact journal: the one whose session header carries
//!   the id.
//!
//! Left out of Crew's driver for now: its hidden supervisor (upstream's
//! process registry reaps agents), the journal writer check and takeover,
//! fork, Google, the Scaffold sandbox profile, the credential broker
//! (`OMP_AUTH_BROKER_*`) and the session-search prompt.

mod rpc;

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::io::{BufRead as _, Read as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;
use zeron_proto::{AgentEvent, HarnessId, Model, ModelProvider, ReasoningLevel, RunRequest, SlashCommand, SteeringMode};

use crate::crew_inference::{ManagedRoute, Provider};
use crate::process::{Command, Stdio};
use crate::{Harness, HarnessError, ModelCatalog, ModelContext, RunControls};

/// The oldest OMP this driver runs: the release Crew's driver was verified
/// against (repeatable `--config`, `--approval-mode`, RPC protocol v2).
pub const MIN_OMP_VERSION: semver::Version = semver::Version::new(17, 2, 9);

/// Command discovery: OMP's startup, then the catalog's settle.
const COMMAND_CATALOG_DEADLINE: Duration = Duration::from_secs(65);
/// `omp models` reads its own catalog: a second, offline.
const MODELS_DEADLINE: Duration = Duration::from_secs(30);
const MODELS_OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;
/// OMP's list of extra config overlays (the user's): passed on as `--config`
/// flags ahead of the run's own overlay, which so comes last, and dropped
/// from the child's environment.
const PI_CONFIG_FILES: &str = "PI_CONFIG_FILES";
const GATEWAY_URL_ENV: &str = "OMP_AUTH_GATEWAY_URL";
const GATEWAY_TOKEN_ENV: &str = "OMP_AUTH_GATEWAY_TOKEN";
/// What an OMP child never inherits: an enclosing Crew run's route (a user's
/// own `crew-auth-gateway.ts` activates on its token) and launcher.
const INHERITED: [&str; 5] = [
    crate::crew_inference::TOKEN_ENV,
    GATEWAY_URL_ENV,
    GATEWAY_TOKEN_ENV,
    "COMET_EXECUTABLE",
    PI_CONFIG_FILES,
];
/// The managed run's providers, on the relay (see the module docs).
const AUTH_GATEWAY: &str = include_str!("auth_gateway.ts");
/// Every role OMP may pick a model for, pinned on a managed run: OMP's
/// roles as Crew pins them. A request outside the route ends the process.
const MODEL_ROLES: [&str; 12] = [
    "default", "smol", "slow", "vision", "plan", "commit", "tiny", "memory", "task", "advisor", "widget", "judge",
];
/// The roles OMP also reads from the environment.
const MODEL_ROLE_ENV: [&str; 3] = ["PI_SMOL_MODEL", "PI_SLOW_MODEL", "PI_PLAN_MODEL"];
/// Loaded into command discovery's disposable OMP, before `session_start`
/// could begin a turn. OMP reports a hook's exception and goes on, so the
/// probe exits instead; the flag makes a failed import a CLI error before
/// any session exists. (Crew's guard.)
const COMMAND_CATALOG_GUARD: &str = r#"import { writeSync } from "node:fs";
export default function (pi) {
  const denyInference = () => {
    const message = "OMP command discovery cannot start inference";
    try {
      writeSync(2, message + "\n");
      writeSync(1, JSON.stringify({ type: "notice", level: "error", source: "command-catalog", message }) + "\n");
    } finally {
      process.exit(1);
    }
  };
  pi.on("before_agent_start", denyInference);
  pi.on("before_provider_request", denyInference);
  pi.registerFlag("comet-command-catalog", { type: "boolean" });
}
"#;
/// Command discovery's overlay: metadata loads, but no skill compression or
/// memory scheduler starts.
const COMMAND_CATALOG_CONFIG: &str = "skillful: false\nmemory:\n  backend: off\n";
const SESSION_HEADER_BYTES: u64 = 64 * 1024;

/// Where the `omp` CLI is: `OMP_EXECUTABLE`, else the usual installs.
pub(crate) fn resolve_executable(executable: Option<PathBuf>) -> Result<PathBuf, HarnessError> {
    if let Some(path) = executable.or_else(|| std::env::var_os("OMP_EXECUTABLE").map(PathBuf::from)) {
        return crate::executable::validate_native_override(&path);
    }
    let home = crate::executable::home_or_current_dir();
    crate::executable::find_on_paths(
        "omp",
        vec![
            home.join(".local/bin/omp"),
            home.join(".bun/bin/omp"),
            PathBuf::from("/opt/homebrew/bin/omp"),
            PathBuf::from("/usr/local/bin/omp"),
        ],
    )
    .ok_or_else(|| HarnessError::NotInstalled("OMP CLI: install OMP or set OMP_EXECUTABLE".into()))
}

pub struct OmpHarness {
    executable: Option<PathBuf>,
    agent_dir: Option<PathBuf>,
    interrupt_grace: Duration,
    kill_grace: Duration,
    models_cache: crate::catalog::Catalog,
    workspace_commands: crate::skills::CommandDiscovery,
}

impl Default for OmpHarness {
    fn default() -> Self {
        Self {
            executable: None,
            agent_dir: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            models_cache: Default::default(),
            workspace_commands: Default::default(),
        }
    }
}

impl OmpHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// A given `omp`, not the one found; its version is not checked.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// OMP's agent directory (`PI_CODING_AGENT_DIR`: its settings, models and
    /// sessions) for the child and for Crew's own reads of it; else OMP's own.
    pub fn with_agent_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.agent_dir = Some(dir.into());
        self
    }

    /// How long an interrupt's `abort` may take, and a stopping process.
    pub fn with_graces(mut self, interrupt: Duration, kill: Duration) -> Self {
        self.interrupt_grace = interrupt;
        self.kill_grace = kill;
        self
    }

    pub fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        resolve_executable(self.executable.clone())
    }

    pub fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }

    /// The executable a launch may use: one found (or named by
    /// `OMP_EXECUTABLE`) must be at least [`MIN_OMP_VERSION`].
    async fn launchable(&self) -> Result<PathBuf, HarnessError> {
        let exe = self.resolve_executable()?;
        if self.executable.is_none() {
            let probed = exe.clone();
            if let Ok(Some(found)) = tokio::task::spawn_blocking(move || version(&probed)).await
                && found < MIN_OMP_VERSION
            {
                return Err(HarnessError::Protocol(format!(
                    "OMP {found} is unsupported; update OMP to {MIN_OMP_VERSION} or newer"
                )));
            }
        }
        Ok(exe)
    }

    fn session_dirs(&self) -> Vec<PathBuf> {
        match &self.agent_dir {
            Some(dir) => vec![dir.join("sessions")],
            None => omp_session_dirs(),
        }
    }

    /// `omp` as Crew launches it: a process group of its own, the user's
    /// PATH, nothing of an enclosing Crew run, and local command semantics.
    fn command(&self, exe: &Path) -> Command {
        let mut cmd = Command::new(exe);
        crate::process::owned::configure(&mut cmd);
        crate::compose_child_path(&mut cmd, exe);
        for key in INHERITED {
            cmd.env_remove(key);
        }
        if let Some(dir) = &self.agent_dir {
            cmd.env("PI_CODING_AGENT_DIR", dir);
        }
        // OMP sets CI for its tools whatever it is launched with: Ashler's
        // repository wrappers read this marker to run as they do locally.
        cmd.env("CI", "false")
            .env("COMET_LOCAL_AGENT_RUNTIME", "1")
            .env("ASHLER_INCREMENTAL_TSC_CHECKS", "false");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// A run's `omp --mode rpc`. Its config overlay, and a managed run's
    /// gateway extension, are written into `scratch`, which outlives the child.
    fn run_command(
        &self,
        exe: &Path,
        request: &RunRequest,
        resume: Option<&Path>,
        managed: Option<&Managed>,
        scratch: &crate::scratch::ScratchDir,
    ) -> Result<Command, HarnessError> {
        let mut cmd = self.command(exe);
        // A managed route also strips every inherited provider credential.
        crate::apply_run_env(&mut cmd, &request.env);
        // The relay's token reaches OMP only as the gateway's own.
        cmd.env_remove(crate::crew_inference::TOKEN_ENV).env_remove(PI_CONFIG_FILES);
        cmd.args(["--mode", "rpc", "--approval-mode", "yolo"]);
        if let Some(journal) = resume {
            cmd.arg("--resume").arg(journal);
        }
        let model = match managed {
            Some(managed) => Some(managed.model.as_str()),
            None => request.model.as_deref().map(str::trim).filter(|model| !model.is_empty() && *model != "default"),
        };
        // Opus 5.5 runs at medium unless asked otherwise, as in Crew.
        let reasoning = request.reasoning.or_else(|| {
            model
                .is_some_and(|model| model.rsplit('/').next() == Some("claude-opus-5-5"))
                .then_some(ReasoningLevel::Medium)
        });
        if let Some(level) = reasoning {
            cmd.args(["--thinking", thinking_flag(level)]);
        }
        let overlay = scratch.path().join("config.yml");
        std::fs::write(&overlay, run_overlay(managed.map(|managed| managed.model.as_str())))?;
        for path in config_overlay_paths(std::env::var_os(PI_CONFIG_FILES).as_deref(), &overlay) {
            cmd.arg("--config").arg(path);
        }
        match managed {
            Some(managed) => {
                let extension = scratch.path().join("agent-auth.ts");
                std::fs::write(&extension, AUTH_GATEWAY)?;
                cmd.arg("--extension").arg(&extension).args(["--model", &managed.model]);
                cmd.env(GATEWAY_URL_ENV, managed.route.base_url.trim_end_matches('/'))
                    .env(GATEWAY_TOKEN_ENV, &managed.route.token);
                for role in MODEL_ROLE_ENV {
                    cmd.env(role, &managed.model);
                }
            }
            None => {
                if let Some(model) = model {
                    cmd.args(["--model", model]);
                }
            }
        }
        if !request.cwd.is_empty() {
            cmd.current_dir(&request.cwd);
        }
        Ok(cmd)
    }

    async fn discover_models(&self) -> Result<Vec<Model>, HarnessError> {
        let exe = self.launchable().await?;
        let mut cmd = self.command(&exe);
        cmd.args(["models", "--json", "--no-extensions"]).stdin(Stdio::null());
        // Crew: a managed engine lists what a managed launch would run.
        crate::crew_inference::managed_probe(HarnessId::Omp, &mut cmd)?;
        let child = crate::process::spawn_agent(&mut cmd)
            .map_err(|e| crate::executable::spawn_error(e, None, || crate::executable::binary_hint(&exe)))?;
        let mut child = crate::process::owned::Child::new(child);
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(HarnessError::Protocol("OMP has no stdio".into()));
        };
        let listed = tokio::time::timeout(MODELS_DEADLINE, async {
            let (stdout, stderr) =
                tokio::join!(rpc::read_bounded(stdout, MODELS_OUTPUT_LIMIT), rpc::read_bounded(stderr, 64 * 1024));
            let status = child.wait().await?;
            Ok::<_, HarnessError>((stdout?, stderr.unwrap_or_default(), status))
        })
        .await;
        child.shutdown(Duration::ZERO).await;
        let (stdout, stderr, status) =
            listed.map_err(|_| HarnessError::Protocol("OMP's model list timed out".into()))??;
        if !status.success() {
            let tail = crate::StderrTail::default();
            for line in String::from_utf8_lossy(&stderr).lines() {
                tail.push(line);
            }
            return Err(HarnessError::Protocol(crate::crash_message("OMP's model list", Some(status), &tail)));
        }
        models_from_catalog(&stdout)
    }

    async fn discover_commands(&self, cwd: &Path) -> Result<Vec<SlashCommand>, HarnessError> {
        let exe = self.launchable().await?;
        let scratch = crate::scratch::ScratchDir::new("omp-command-catalog")?;
        let guard = scratch.path().join("guard.js");
        std::fs::write(&guard, COMMAND_CATALOG_GUARD)?;
        let config = scratch.path().join("config.yml");
        std::fs::write(&config, COMMAND_CATALOG_CONFIG)?;
        let mut cmd = self.command(&exe);
        crate::crew_inference::managed_probe(HarnessId::Omp, &mut cmd)?;
        cmd.args(["--mode", "rpc", "--approval-mode", "yolo", "--no-session", "--comet-command-catalog"])
            .arg("--extension")
            .arg(&guard);
        for path in config_overlay_paths(std::env::var_os(PI_CONFIG_FILES).as_deref(), &config) {
            cmd.arg("--config").arg(path);
        }
        cmd.current_dir(cwd);
        let child = crate::process::spawn_agent(&mut cmd)
            .map_err(|e| crate::executable::spawn_error(e, Some(cwd), || crate::executable::binary_hint(&exe)))?;
        rpc::command_catalog(rpc::Process::new(child, Some(scratch))?, COMMAND_CATALOG_DEADLINE).await
    }
}

/// A managed run's route, and the model it runs: `comet-{provider}/{id}`.
struct Managed {
    route: ManagedRoute,
    model: String,
}

/// The route in `request`'s environment and the model OMP runs on it; none
/// for an unmanaged run. A route that's unsound, or a model of another
/// provider than the route's, stops the launch.
fn managed(request: &RunRequest) -> Result<Option<Managed>, HarnessError> {
    crate::crew_inference::validate(HarnessId::Omp, &request.env)?;
    let Some(route) = ManagedRoute::from_env(&request.env) else {
        return Ok(None);
    };
    let refused = |why: String| HarnessError::Protocol(format!("Crew can't run OMP on its shared accounts: {why}"));
    // `prepare` names the route's model, its provider prefix gone; a
    // request may still carry it.
    let model = request
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty() && *model != "default")
        .ok_or_else(|| refused("the run names no model".into()))?;
    if model.contains('/') && Provider::for_model(HarnessId::Omp, model) != Some(route.provider) {
        return Err(refused(format!("{model} isn't a model of its {} route", route.provider.as_str())));
    }
    let id = model.rsplit('/').next().unwrap_or(model);
    if id.is_empty() || id.len() > 256 || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(refused(format!("{model} isn't a model id")));
    }
    Ok(Some(Managed {
        model: format!("comet-{}/{id}", route.provider.as_str()),
        route,
    }))
}

/// Every run's config overlay, read last: screenshots kept at OMP's
/// coordinate-safe size (Agent Auth takes at most 8 MiB a request, and OMP's
/// 3840x2400 default keeps multi-megabyte PNGs in history); and, on a managed
/// run, every model role on the routed model.
fn run_overlay(managed_model: Option<&str>) -> String {
    let mut overlay = json!({ "computer": { "maxWidth": 1280, "maxHeight": 896 } });
    if let Some(model) = managed_model {
        let roles: Map<String, Value> = MODEL_ROLES.iter().map(|role| ((*role).to_owned(), json!(model))).collect();
        overlay["modelRoles"] = Value::Object(roles);
    }
    serde_yaml_ng::to_string(&overlay).expect("a JSON value serializes as YAML")
}

/// `PI_CONFIG_FILES`' overlays, then the run's own, as `--config` flags.
fn config_overlay_paths(inherited: Option<&OsStr>, run_config: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = inherited.map(|value| std::env::split_paths(value).collect()).unwrap_or_default();
    paths.push(run_config.to_path_buf());
    paths
}

/// OMP's `--thinking` ladder (off, minimal, low, medium, high, xhigh, max):
/// the levels above it run at max.
fn thinking_flag(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max | ReasoningLevel::Ultra | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => "max",
    }
}

/// `omp models --json`'s rows as models: `{provider}/{id}` (else the row's
/// `selector`), named as OMP names them, with the thinking levels each lists.
/// Only chat models; none is marked default.
fn models_from_catalog(bytes: &[u8]) -> Result<Vec<Model>, HarnessError> {
    let catalog: Value = serde_json::from_slice(bytes)
        .map_err(|error| HarnessError::Protocol(format!("OMP's model list is not JSON: {error}")))?;
    let rows = catalog
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| HarnessError::Protocol("OMP's model list has no models".into()))?;
    Ok(rows.iter().filter_map(model_from_row).collect())
}

fn model_from_row(row: &Value) -> Option<Model> {
    let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty());
    if text("kind").is_some_and(|kind| kind != "chat") {
        return None;
    }
    let (provider, id) = match (text("provider"), text("id")) {
        (Some(provider), Some(id)) => (provider.to_owned(), id.to_owned()),
        _ => {
            let (provider, id) = text("selector")?.split_once('/')?;
            (provider.to_owned(), id.to_owned())
        }
    };
    // Crew's ladder for the models it was tuned on: low through max.
    let max_ladder = matches!(
        id.as_str(),
        "claude-opus-5-5" | "gpt-6-sol" | "gpt-6.1-sol" | "gpt-6.2-sol" | "gpt-6.2-astra" | "gpt-6-luna"
    );
    let reasoning_levels = row
        .get("thinking")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|level| serde_json::from_value::<ReasoningLevel>(level.clone()).ok())
        .filter(|level| {
            !max_ladder
                || matches!(
                    level,
                    ReasoningLevel::Low | ReasoningLevel::Medium | ReasoningLevel::High | ReasoningLevel::XHigh | ReasoningLevel::Max
                )
        })
        .collect();
    Some(Model {
        label: text("name").unwrap_or(&id).to_owned(),
        provider: Some(ModelProvider {
            name: crate::pi::provider_name(&provider),
            id: provider.clone(),
        }),
        id: format!("{provider}/{id}"),
        description: None,
        reasoning_levels,
        options: Vec::new(),
        default: false,
    })
}

// ---------------------------------------------------------------------------
// OMP's directories and journals
// ---------------------------------------------------------------------------

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|value| !value.is_empty()).map(PathBuf::from)
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
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        })
}

/// The profile OMP runs under (`OMP_PROFILE`, else `PI_PROFILE`).
fn profile() -> Option<String> {
    std::env::var("OMP_PROFILE")
        .ok()
        .or_else(|| std::env::var("PI_PROFILE").ok())
        .filter(|profile| valid_profile_name(profile))
}

/// OMP's agent directory: its settings, models and sessions.
fn agent_dir(home: &Path, profile: Option<&str>) -> PathBuf {
    if profile.is_none()
        && let Some(dir) = env_path("PI_CODING_AGENT_DIR")
    {
        return dir;
    }
    let root = home.join(std::env::var_os("PI_CONFIG_DIR").filter(|value| !value.is_empty()).unwrap_or_else(|| ".omp".into()));
    match profile {
        Some(name) => root.join("profiles").join(name).join("agent"),
        None => root.join("agent"),
    }
}

/// Where OMP keeps its session journals, as Crew's driver looks for them.
fn omp_session_dirs() -> Vec<PathBuf> {
    let Some(home) = crate::executable::home_dir() else {
        return Vec::new();
    };
    let profile = profile();
    let mut dirs = Vec::with_capacity(2);
    if let Some(xdg) = env_path("XDG_DATA_HOME") {
        let root = xdg.join("omp");
        let root = match profile.as_deref() {
            Some(name) => root.join("profiles").join(name),
            None => root,
        };
        if root.is_dir() {
            dirs.push(root.join("sessions"));
        }
    }
    dirs.push(agent_dir(&home, profile.as_deref()).join("sessions"));
    dirs
}

/// A journal's session id: its `session` header's, within its first 64 KiB.
fn session_file_id(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file.take(SESSION_HEADER_BYTES));
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
        if let Ok(record) = serde_json::from_str::<Value>(&line)
            && record.get("type").and_then(Value::as_str) == Some("session")
        {
            return record
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty() && Path::new(id).components().count() == 1)
                .map(str::to_owned);
        }
        line.clear();
    }
    None
}

/// The one journal `selector` names (its path, or the session id its header
/// carries), as its canonical path and session id; else why not. Every
/// journal under `dirs` is read: piled-up sessions must not hide one.
fn resolve_session(dirs: &[PathBuf], selector: &str) -> Result<(String, PathBuf), String> {
    let direct = Path::new(selector);
    let found = if direct.is_file() {
        vec![direct.to_path_buf()]
    } else {
        let mut found = Vec::new();
        let mut pending: VecDeque<PathBuf> = dirs.iter().cloned().collect();
        while let Some(dir) = pending.pop_front() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("{} could not be read: {error}", dir.display())),
            };
            for entry in entries {
                let entry = entry.map_err(|error| format!("{} could not be read: {error}", dir.display()))?;
                // Entry types don't follow symlinks.
                let file_type = entry.file_type().map_err(|error| error.to_string())?;
                let path = entry.path();
                if file_type.is_dir() {
                    pending.push_back(path);
                } else if file_type.is_file()
                    && path.extension().is_some_and(|extension| extension == "jsonl")
                    && session_file_id(&path).as_deref() == Some(selector)
                {
                    found.push(path);
                }
            }
        }
        found
    };
    match found.as_slice() {
        [] => Err("no journal of it was found".into()),
        [journal] => {
            let journal = std::fs::canonicalize(journal).map_err(|error| error.to_string())?;
            let id = session_file_id(&journal).ok_or("its journal has no session header")?;
            Ok((id, journal))
        }
        _ => Err("more than one journal claims it".into()),
    }
}

// ---------------------------------------------------------------------------
// OMP's version
// ---------------------------------------------------------------------------

type VersionKey = (PathBuf, Option<SystemTime>, u64);

/// OMP's version, which it prints as `omp/18.22.1` (upstream's
/// `binary_version` can't read that). Kept per executable file, as upstream
/// keeps its probes; a probe that got no answer is tried again next time.
fn version(exe: &Path) -> Option<semver::Version> {
    static CACHE: OnceLock<Mutex<HashMap<VersionKey, Option<semver::Version>>>> = OnceLock::new();
    let canonical = exe.canonicalize().ok()?;
    let metadata = canonical.metadata().ok()?;
    let key = (canonical, metadata.modified().ok(), metadata.len());
    let cache = CACHE.get_or_init(Default::default);
    if let Some(version) = cache.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
        return version.clone();
    }
    let answered = probe_version(&key.0)?;
    cache.lock().unwrap_or_else(PoisonError::into_inner).insert(key, answered.clone());
    answered
}

/// `omp --version`'s answer (`None` inside: it named no version); `None`
/// when it gave none in time.
fn probe_version(exe: &Path) -> Option<Option<semver::Version>> {
    let mut command = std::process::Command::new(exe);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut printed = String::new();
    child.stdout.take()?.take(4096).read_to_string(&mut printed).ok()?;
    Some(parse_version(&printed))
}

fn parse_version(printed: &str) -> Option<semver::Version> {
    printed
        .split(|c: char| c.is_whitespace() || c == '/')
        .find_map(|word| semver::Version::parse(word.trim_start_matches('v')).ok())
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
    /// Prompts carry `streamingBehavior: "steer"`: OMP takes one between the
    /// live turn's tool calls, or starts the next turn with it.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    /// Per model: each row of `omp models` lists its own.
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }
    fn executable_path(&self) -> Option<PathBuf> {
        self.resolve_executable().ok()
    }
    /// Every turn ends on OMP's own terminal `agent_end`, one it began itself too.
    fn deterministic_turn_end(&self) -> bool {
        true
    }
    /// A stop aborts the turn; the session stays up for the next prompt.
    fn stops_turn_in_place(&self) -> bool {
        true
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(self.model_catalog(false).await?.models)
    }
    /// OMP's binary, its agent directory's settings and models, and the
    /// variables it and its providers read (upstream's table has no row for
    /// OMP). Its login database changes as sessions run, so it isn't read.
    fn model_context(&self) -> Result<Option<ModelContext>, HarnessError> {
        let exe = self.resolve_executable()?;
        let files: Vec<PathBuf> = self
            .agent_dir
            .clone()
            .or_else(|| crate::executable::home_dir().map(|home| agent_dir(&home, profile().as_deref())))
            .into_iter()
            .flat_map(|dir| ["config.yml", "models.yml", "models.json", "settings.json", "auth.json"].map(|name| dir.join(name)))
            .collect();
        let mut context = crate::model_context::context(HarnessId::Omp, &exe, &files)?;
        let mut hash = Sha256::new();
        hash.update(context.hash.as_bytes());
        let mut env: Vec<_> = std::env::vars_os()
            .filter(|(key, _)| {
                let key = key.to_string_lossy();
                ["OMP_", "PI_", "ANTHROPIC_", "OPENAI_"].iter().any(|prefix| key.starts_with(prefix))
            })
            .collect();
        env.sort();
        for (key, value) in env {
            for field in [key.as_encoded_bytes(), value.as_encoded_bytes()] {
                hash.update((field.len() as u64).to_le_bytes());
                hash.update(field);
            }
        }
        context.hash = format!("{:x}", hash.finalize());
        context.binary_version = version(&context.binary_path).map(|version| version.to_string());
        Ok(Some(context))
    }
    async fn model_catalog(&self, force: bool) -> Result<ModelCatalog, HarnessError> {
        self.models_cache
            .get_with_timeout(
                force,
                MODELS_DEADLINE + Duration::from_secs(5),
                || Ok(self.model_context()?.expect("OMP's model context").key()),
                || self.discover_models(),
            )
            .await
    }
    /// What a run that names no model gets when OMP's list can't be read:
    /// OMP's own default.
    fn fallback_models(&self) -> Vec<Model> {
        vec![Model {
            id: "default".into(),
            label: "OMP default".into(),
            description: Some("Model configured in OMP".into()),
            provider: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
            default: false,
        }]
    }
    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        self.commands_for(&std::env::current_dir()?).await
    }
    async fn commands_for(&self, cwd: &Path) -> Result<Vec<SlashCommand>, HarnessError> {
        self.workspace_commands.get(cwd, self.discover_commands(cwd)).await
    }
    async fn run(
        &self,
        mut request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let managed = managed(&request)?;
        let exe = self.launchable().await?;
        let mut resume = None;
        let mut lost_context = None;
        if let Some(selector) = request.resume.take() {
            let dirs = self.session_dirs();
            let lookup = selector.clone();
            let found = tokio::task::spawn_blocking(move || resolve_session(&dirs, &lookup))
                .await
                .map_err(|error| HarnessError::Protocol(format!("OMP's journal lookup failed: {error}")))?;
            match found {
                Ok((id, journal)) => {
                    request.resume = Some(id.clone());
                    resume = Some((id, journal));
                }
                // As upstream's other agents do: a session that can't be
                // reopened starts afresh and says so. Failing would strand the
                // chat on an id it resumes forever.
                Err(why) => {
                    lost_context = Some(format!(
                        "OMP could not restore session {selector} ({why}); starting a new session without the earlier context"
                    ));
                }
            }
        }
        let scratch = crate::scratch::ScratchDir::new("omp-run")?;
        let mut cmd = self.run_command(
            &exe,
            &request,
            resume.as_ref().map(|(_, journal)| journal.as_path()),
            managed.as_ref(),
            &scratch,
        )?;
        let cwd = (!request.cwd.is_empty()).then(|| Path::new(&request.cwd));
        let child = crate::process::spawn_agent(&mut cmd)
            .map_err(|e| crate::executable::spawn_error(e, cwd, || crate::executable::binary_hint(&exe)))?;
        let process = rpc::Process::new(child, Some(scratch))?;
        let (events, receiver) = mpsc::channel(256);
        let options = rpc::Options {
            expected_resume: resume.map(|(id, _)| id),
            lost_context,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            managed: managed.is_some(),
        };
        tokio::spawn(rpc::run(process, request, controls, events, options));
        Ok(futures::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|event| (event, receiver))
        })
        .boxed())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::crew_inference::ROUTE_ENV;

    fn request(model: Option<&str>) -> RunRequest {
        RunRequest {
            prompt: "hello".into(),
            harness: None,
            model: model.map(str::to_owned),
            reasoning: None,
            model_options: Default::default(),
            cwd: String::new(),
            sandbox: zeron_proto::SandboxLevel::DangerFullAccess,
            auto_approve: true,
            access: Default::default(),
            plan: false,
            resume: None,
            attachments: Vec::new(),
            worktree: None,
            mcp: None,
            extra_mcp: Vec::new(),
            env: Default::default(),
        }
    }

    fn route(provider: Provider) -> BTreeMap<String, String> {
        ManagedRoute {
            provider,
            base_url: "http://127.0.0.1:9876".into(),
            token: "opaque-local-token".into(),
        }
        .env()
    }

    fn args(cmd: &Command) -> Vec<String> {
        cmd.as_std().get_args().map(|arg| arg.to_string_lossy().into_owned()).collect()
    }

    fn envs(cmd: &Command) -> BTreeMap<String, Option<String>> {
        cmd.as_std()
            .get_envs()
            .map(|(key, value)| {
                (key.to_string_lossy().into_owned(), value.map(|value| value.to_string_lossy().into_owned()))
            })
            .collect()
    }

    fn after<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.windows(2).filter(|pair| pair[0] == flag).map(|pair| pair[1].as_str()).collect()
    }

    /// What `omp models --json` printed offline on 18.22.1 with placeholder
    /// keys (abridged), plus a row of another kind and one from an older
    /// OMP that had only `selector`.
    #[test]
    fn the_model_list_is_omp_s_rows_as_provider_slash_id() {
        let listed = br#"{"models":[
            {"provider":"anthropic","kind":"chat","id":"claude-opus-5-5","selector":"anthropic/claude-opus-5-5","name":"Claude Opus 5.5","contextWindow":1000000,"maxTokens":128000,"reasoning":true,"thinking":["minimal","low","medium","high","xhigh","max"],"cost":{"input":4,"output":20}},
            {"provider":"openai","kind":"chat","id":"gpt-6.2-sol","selector":"openai/gpt-6.2-sol","name":"GPT-6.2 Sol","reasoning":true,"thinking":["low","medium","high","xhigh","max"]},
            {"provider":"openai","kind":"chat","id":"gpt-4o","name":"GPT-4o","reasoning":false},
            {"provider":"openai","kind":"embedding","id":"text-embedding-3","name":"Embedding"},
            {"selector":"openai-codex/gpt-5.6-sol","name":"GPT-5.6 Sol","thinking":["off","low","auto"]}
        ]}"#;
        let models = models_from_catalog(listed).unwrap();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["anthropic/claude-opus-5-5", "openai/gpt-6.2-sol", "openai/gpt-4o", "openai-codex/gpt-5.6-sol"]);
        let opus = &models[0];
        assert_eq!(opus.label, "Claude Opus 5.5");
        assert_eq!(opus.provider.as_ref().map(|provider| provider.name.as_str()), Some("Anthropic"));
        // Crew's ladder: no minimal for the models it was tuned on.
        assert_eq!(
            opus.reasoning_levels,
            [ReasoningLevel::Low, ReasoningLevel::Medium, ReasoningLevel::High, ReasoningLevel::XHigh, ReasoningLevel::Max]
        );
        assert!(models[2].reasoning_levels.is_empty());
        // OMP's own levels that are no reasoning level drop out.
        assert_eq!(models[3].reasoning_levels, [ReasoningLevel::Low]);
        assert!(models.iter().all(|model| !model.default && model.options.is_empty()));
        // Each is routable as Crew's engine routes OMP: by its provider.
        assert_eq!(Provider::for_model(HarnessId::Omp, &models[0].id), Some(Provider::Anthropic));
        assert_eq!(Provider::for_model(HarnessId::Omp, &models[1].id), Some(Provider::Openai));
        assert!(models_from_catalog(b"{}").is_err());
    }

    /// A managed run reaches the relay through the gateway alone: its model
    /// and every role on `comet-{provider}/{id}`, the gateway's variables,
    /// no `COMET_INFERENCE_TOKEN`, and no inherited credential.
    #[test]
    fn a_managed_launch_reaches_only_the_relay() {
        let harness = OmpHarness::new();
        let scratch = crate::scratch::ScratchDir::new("omp-test").unwrap();
        // As `prepare` leaves it: the route's model, without its provider.
        let mut run = request(Some("claude-opus-5-5"));
        run.env = route(Provider::Anthropic);
        run.env.insert("GH_TOKEN".into(), "the-host's".into());
        let launch = managed(&run).unwrap().unwrap();
        assert_eq!(launch.model, "comet-anthropic/claude-opus-5-5");
        let cmd = harness.run_command(Path::new("omp"), &run, None, Some(&launch), &scratch).unwrap();
        let argv = args(&cmd);
        assert_eq!(&argv[..4], ["--mode", "rpc", "--approval-mode", "yolo"]);
        assert_eq!(after(&argv, "--model"), ["comet-anthropic/claude-opus-5-5"]);
        assert_eq!(after(&argv, "--thinking"), ["medium"], "Opus 5.5's default, as Crew's");
        let extension = after(&argv, "--extension");
        assert_eq!(extension.len(), 1);
        assert_eq!(std::fs::read_to_string(extension[0]).unwrap(), AUTH_GATEWAY);
        let overlay = after(&argv, "--config").last().map(|path| std::fs::read_to_string(path).unwrap()).unwrap();
        let overlay: Value = serde_yaml_ng::from_str(&overlay).unwrap();
        assert_eq!(overlay["computer"], json!({"maxWidth": 1280, "maxHeight": 896}));
        for role in MODEL_ROLES {
            assert_eq!(overlay["modelRoles"][role], "comet-anthropic/claude-opus-5-5", "{role}");
        }
        let env = envs(&cmd);
        assert_eq!(env[GATEWAY_URL_ENV].as_deref(), Some("http://127.0.0.1:9876"));
        assert_eq!(env[GATEWAY_TOKEN_ENV].as_deref(), Some("opaque-local-token"));
        for role in MODEL_ROLE_ENV {
            assert_eq!(env[role].as_deref(), Some("comet-anthropic/claude-opus-5-5"));
        }
        for gone in [crate::crew_inference::TOKEN_ENV, PI_CONFIG_FILES, "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"] {
            assert_eq!(env.get(gone), Some(&None), "{gone} must be removed");
        }
        assert_eq!(env[ROUTE_ENV].as_deref(), Some("anthropic"));
        assert_eq!(env["GH_TOKEN"].as_deref(), Some("the-host's"));
        assert_eq!(env["CI"].as_deref(), Some("false"));
        assert!(!argv.iter().any(|arg| arg.contains("opaque-local-token")), "the token never rides argv");

        // OpenAI's route, the model still prefixed; and one that isn't the route's.
        let mut run = request(Some("openai/gpt-6.2-sol"));
        run.env = route(Provider::Openai);
        assert_eq!(managed(&run).unwrap().unwrap().model, "comet-openai/gpt-6.2-sol");
        run.env = route(Provider::Anthropic);
        assert!(managed(&run).is_err());
        run.model = None;
        assert!(managed(&run).is_err(), "a managed run names its model");
        // A broken route never launches.
        run.env.remove(crate::crew_inference::URL_ENV);
        assert!(managed(&run).is_err());
    }

    #[test]
    fn an_unmanaged_launch_runs_omp_s_own_configuration() {
        let harness = OmpHarness::new();
        let scratch = crate::scratch::ScratchDir::new("omp-test").unwrap();
        let mut run = request(Some("anthropic/claude-sonnet-5-5"));
        run.reasoning = Some(ReasoningLevel::Ultrathink);
        assert!(managed(&run).unwrap().is_none());
        let resume = scratch.path().join("journal.jsonl");
        let cmd = harness.run_command(Path::new("omp"), &run, Some(&resume), None, &scratch).unwrap();
        let argv = args(&cmd);
        assert_eq!(after(&argv, "--model"), ["anthropic/claude-sonnet-5-5"]);
        assert_eq!(after(&argv, "--thinking"), ["max"]);
        assert_eq!(after(&argv, "--resume"), [resume.to_str().unwrap()]);
        assert!(after(&argv, "--extension").is_empty());
        let overlay = std::fs::read_to_string(after(&argv, "--config").last().unwrap()).unwrap();
        assert!(!overlay.contains("modelRoles"), "{overlay}");
        let env = envs(&cmd);
        assert!(!env.contains_key(GATEWAY_URL_ENV) || env[GATEWAY_URL_ENV].is_none());
        // OMP's own default model: no `--model` at all.
        let cmd = harness.run_command(Path::new("omp"), &request(Some("default")), None, None, &scratch).unwrap();
        assert!(after(&args(&cmd), "--model").is_empty());
    }

    #[test]
    fn a_resume_finds_the_exact_journal_by_its_session_header() {
        let dir = tempfile::tempdir().unwrap();
        let journal = |relative: &str, id: &str| {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("{}\n{{\"type\":\"message\"}}\n", json!({"type": "session", "id": id, "cwd": "/repo"}))).unwrap();
            path
        };
        let wanted = journal("--repo--/2026-11-20_wanted.jsonl", "wanted");
        journal("--other--/nested/2026-11-19_other.jsonl", "other");
        journal("--repo--/not-a-journal.txt", "wanted");
        let dirs = [dir.path().to_path_buf(), dir.path().join("missing")];
        let canonical = std::fs::canonicalize(&wanted).unwrap();
        assert_eq!(resolve_session(&dirs, "wanted"), Ok(("wanted".into(), canonical.clone())));
        assert_eq!(resolve_session(&dirs, wanted.to_str().unwrap()), Ok(("wanted".into(), canonical)));
        assert!(resolve_session(&dirs, "absent").is_err());
        journal("--copy--/2026-11-20_wanted.jsonl", "wanted");
        assert_eq!(resolve_session(&dirs, "wanted"), Err("more than one journal claims it".into()));
        let headerless = dir.path().join("headerless.jsonl");
        std::fs::write(&headerless, "{\"type\":\"message\"}\n").unwrap();
        assert!(resolve_session(&dirs, headerless.to_str().unwrap()).is_err());
    }

    /// OMP logs a hook's exception and goes on: the gateway's refusals end
    /// the process instead.
    #[test]
    fn the_gateway_exits_rather_than_throws() {
        let code: String = AUTH_GATEWAY
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.contains("throw "), "a thrown refusal would be logged and ignored");
        assert!(code.contains("process.exit(1)"));
        assert!(code.contains("\"before_provider_request\""));
        assert!(code.contains("OMP_AUTH_GATEWAY_TOKEN") && code.contains("OMP_AUTH_GATEWAY_URL"));
        assert!(!code.contains("COMET_INFERENCE_TOKEN"));
    }

    #[test]
    fn thinking_levels_and_versions_read_as_omp_s() {
        assert_eq!(thinking_flag(ReasoningLevel::XHigh), "xhigh");
        assert_eq!(thinking_flag(ReasoningLevel::Ultracode), "max");
        assert_eq!(parse_version("omp/18.22.1\n"), Some(semver::Version::new(18, 22, 1)));
        assert_eq!(parse_version("omp v17.2.9"), Some(MIN_OMP_VERSION));
        assert_eq!(parse_version("omp"), None);
        assert!(semver::Version::new(17, 2, 8) < MIN_OMP_VERSION);
        let inherited = std::env::join_paths(["/a.yml", "/b.yml"]).unwrap();
        assert_eq!(
            config_overlay_paths(Some(&inherited), Path::new("/run.yml")),
            ["/a.yml", "/b.yml", "/run.yml"].map(PathBuf::from)
        );
    }

    /// The version floor reads OMP's own answer, kept per executable file.
    #[cfg(unix)]
    #[test]
    fn the_version_floor_reads_omp_s_answer() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("omp");
        std::fs::write(&fake, "#!/bin/sh\necho omp/17.2.8\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(version(&fake), Some(semver::Version::new(17, 2, 8)));
        assert_eq!(version(&fake), Some(semver::Version::new(17, 2, 8)));
    }
}
