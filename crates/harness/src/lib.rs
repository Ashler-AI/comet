//! comet-harness — one interface over Claude Code / Codex (and a mock for tests).
//!
//! Integration decisions (docs/research/harness.md):
//! - Claude Code: spawn the installed `claude` CLI with
//!   `--input-format stream-json --output-format stream-json --verbose
//!    --include-partial-messages`, implement the control channel (can_use_tool →
//!   requestInput, interrupt, set_model), steer by writing user lines mid-run.
//! - Codex: spawn `codex app-server`, JSON-RPC 2.0 over stdio (thread/start, turn/start,
//!   turn/steer{expectedTurnId}, turn/interrupt, item/* + delta notifications).

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::{mpsc, oneshot};
pub use tokio_util::sync::CancellationToken;

use comet_proto::{
    AgentEvent, HarnessCommand, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("harness binary not found: {0}")]
    NotInstalled(String),
    #[error("harness protocol error: {0}")]
    Protocol(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A steer prompt pushed into a live run; delivered at the harness's steering boundary.
#[derive(Debug, Clone)]
pub struct SteerMessage {
    pub prompt: String,
    pub message_id: Option<String>,
}
#[derive(Clone, PartialEq, Eq)]
pub struct InferenceRoute {
    pub base_url: String,
    pub token: String,
    pub provider: String,
    pub model: String,
}

impl std::fmt::Debug for InferenceRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceRoute")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .finish()
    }
}

/// Engine-owned context exposed to an agent child process. This is launch
/// metadata, not user-authored run configuration or prompt content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContext {
    pub session_id: String,
    pub ipc_port: u16,
    pub inference: Option<InferenceRoute>,
}

/// Host-side controls handed to a run: input-request bridge + steering mailbox.
pub struct RunControls {
    /// The run sends questions and awaits answers (blocks the agent, mirrors comet).
    pub request_input: Box<
        dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync,
    >,
    /// Steer prompts consumed at step/turn boundaries.
    pub steering: mpsc::Receiver<SteerMessage>,
    /// Cancel to interrupt the live run: the harness sends its protocol-level
    /// interrupt, then escalates to SIGTERM/SIGKILL on the child after a grace
    /// period. The run's stream ends with `Done { status: Interrupted }`.
    pub interrupt: CancellationToken,
    /// Current Comet session and loopback RPC port for session CLI calls.
    /// Non-session utility runs (for example title generation) leave this unset.
    pub context: Option<RunContext>,
}

#[async_trait]
pub trait Harness: Send + Sync {
    fn id(&self) -> HarnessId;
    fn display_name(&self) -> &str;
    fn supports_steering(&self) -> bool;
    fn steering_mode(&self) -> SteeringMode;
    fn reasoning_levels(&self) -> &[ReasoningLevel];
    async fn models(&self) -> Result<Vec<Model>, HarnessError>;
    async fn commands(&self, _cwd: &str) -> Result<Vec<HarnessCommand>, HarnessError> {
        Ok(Vec::new())
    }
    /// Run one (persistent) session; the stream ends with `AgentEvent::Done`.
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError>;
}

mod approval;
mod auth_gateway;
pub mod claude;
pub mod codex;
pub mod mock;
pub mod omp;
pub mod prime_agent;
pub mod shell_env;

/// Product-curated model set shared by ACP-backed harness catalogs.
///
/// Exact ids keep renamed/legacy variants out. GPT-5.6 is admitted by family
/// prefix because both Codex and Prime publish multiple current profiles;
/// OpenRouter's official DeepSeek namespace is admitted as a family so new
/// first-party DeepSeek releases appear without also admitting repackages.
pub(crate) fn is_curated_comet_model(model_id: &str) -> bool {
    let is_gpt_56_family = |prefix: &str| {
        model_id == prefix
            || model_id
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('-'))
    };
    let is_openrouter_deepseek = model_id
        .strip_prefix("openrouter/")
        .is_some_and(|id| id.starts_with("deepseek/") || id.starts_with("~deepseek/"));
    is_gpt_56_family("openai-codex/gpt-5.6")
        || is_gpt_56_family("prime-inference/openai/gpt-5.6")
        || is_openrouter_deepseek
        || matches!(
            model_id,
            "anthropic/claude-opus-5"
                | "anthropic/claude-sonnet-5"
                | "anthropic/claude-fable-5"
                | "prime-inference/anthropic/claude-opus-5"
                | "prime-inference/anthropic/claude-sonnet-5"
                | "prime-inference/anthropic/claude-fable-5"
                | "prime-inference/moonshotai/kimi-k3"
                | "prime-inference/x-ai/grok-4.20"
                | "prime-inference/x-ai/grok-4.20-multi-agent"
        )
}

/// Bin directories where npm-installed CLIs land under Node version managers.
/// GUI launches never see these on PATH — the managers shape PATH in shell
/// init (fnm's per-shell multishells, nvm's shell function), which a
/// Dock/Finder-launched app never runs.
pub(crate) fn node_version_manager_bins() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = Vec::new();
    // fnm: `aliases/default` is a stable symlink to the active default
    // installation (the multishell PATH entries are ephemeral, per-shell).
    let mut fnm_roots: Vec<PathBuf> = std::env::var_os("FNM_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Some(home) = &home {
        fnm_roots.push(home.join(".local").join("share").join("fnm"));
        fnm_roots.push(home.join("Library").join("Application Support").join("fnm"));
        fnm_roots.push(home.join(".fnm"));
    }
    for root in fnm_roots {
        dirs.push(root.join("aliases").join("default").join("bin"));
    }
    if let Some(home) = &home {
        // volta / bun keep real shims in a fixed bin dir; pnpm has a global bin.
        dirs.push(home.join(".volta").join("bin"));
        dirs.push(home.join(".bun").join("bin"));
        dirs.push(home.join("Library").join("pnpm"));
        dirs.push(home.join(".local").join("share").join("pnpm"));
        // nvm: every installed version's bin, newest first.
        let nvm = home.join(".nvm").join("versions").join("node");
        if let Ok(entries) = std::fs::read_dir(&nvm) {
            let mut versions: Vec<PathBuf> =
                entries.flatten().map(|e| e.path().join("bin")).collect();
            versions.sort();
            versions.reverse();
            dirs.append(&mut versions);
        }
    }
    dirs
}

/// Compose the child's PATH: the resolved executable's directory first, then
/// our own PATH, then the login-shell PATH snapshot — deduped. npm-shim CLIs
/// are `#!/usr/bin/env node` scripts whose `node` lives beside them in the
/// version manager's bin dir, and the CLIs themselves shell out to tools
/// (git, rg, node) that a GUI/service launch's own PATH may lack.
pub(crate) fn compose_child_path(cmd: &mut tokio::process::Command, exe: &std::path::Path) {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if let Some(dir) = exe.parent().filter(|d| !d.as_os_str().is_empty()) {
        paths.push(dir.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Some(shell_path) = shell_env::login_shell_path() {
        paths.extend(std::env::split_paths(shell_path));
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| !p.as_os_str().is_empty() && seen.insert(p.clone()));
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env("PATH", joined);
    }
}
pub(crate) fn apply_run_context(cmd: &mut tokio::process::Command, context: Option<&RunContext>) {
    // Never inherit a stale context from the engine's own launch environment.
    cmd.env_remove("COMET_SESSION_ID")
        .env_remove("COMET_IPC_PORT")
        .env_remove("COMET_INFERENCE_TOKEN")
        .env_remove("PRIME_AGENT_AUTH_GATEWAY_URL")
        .env_remove("OMP_AUTH_GATEWAY_URL")
        .env_remove("OMP_AUTH_GATEWAY_TOKEN");
    if let Some(context) = context {
        cmd.env("COMET_SESSION_ID", &context.session_id)
            .env("COMET_IPC_PORT", context.ipc_port.to_string());
        if let Some(inference) = &context.inference {
            // OMP_AUTH_BROKER_* is OMP's credential-vault protocol, not an
            // inference gateway. Shared Comet runs instead load the bundled
            // provider adapter below and must not leak an inherited vault.
            cmd.env_remove("OMP_AUTH_BROKER_URL")
                .env_remove("OMP_AUTH_BROKER_TOKEN")
                .env("COMET_INFERENCE_TOKEN", &inference.token)
                .env("PRIME_AGENT_AUTH_GATEWAY_URL", &inference.base_url)
                .env("OMP_AUTH_GATEWAY_URL", &inference.base_url)
                .env("OMP_AUTH_GATEWAY_TOKEN", &inference.token);
            match inference.provider.as_str() {
                "openai" => {
                    cmd.env("OPENAI_BASE_URL", format!("{}/v1", inference.base_url))
                        .env("OPENAI_API_KEY", &inference.token)
                        .env_remove("CODEX_API_KEY");
                }
                "anthropic" => {
                    cmd.env("ANTHROPIC_BASE_URL", &inference.base_url)
                        .env("ANTHROPIC_AUTH_TOKEN", &inference.token)
                        .env_remove("ANTHROPIC_API_KEY");
                }
                _ => {}
            }
        }
    }
}

/// Rolling tail of a child's stderr, shared between the reader task and the
/// crash-message composer: an unexpected exit surfaces "<name> exited
/// unexpectedly (<status>): <last stderr lines>" instead of a bare shrug —
/// the proper background-crash message old comet showed (user requirement).
#[derive(Clone, Default)]
pub(crate) struct StderrTail(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>);

impl StderrTail {
    const KEEP_LINES: usize = 6;
    const KEEP_BYTES: usize = 700;

    pub(crate) fn push(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let mut tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tail.push_back(line.chars().take(Self::KEEP_BYTES).collect());
        while tail.len() > Self::KEEP_LINES {
            tail.pop_front();
        }
    }

    /// The captured tail as one display string, `None` when nothing arrived.
    pub(crate) fn snapshot(&self) -> Option<String> {
        let tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tail.is_empty() {
            return None;
        }
        let mut joined = tail.iter().cloned().collect::<Vec<_>>().join("\n");
        joined.truncate(Self::KEEP_BYTES * 2);
        Some(joined)
    }
}

/// "exit code 137" / "signal 9 (killed)" / "unknown" — the status half of a
/// crash message, from a `try_wait` result after the stream ended.
pub(crate) fn describe_exit(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "still running".into();
    };
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    "unknown exit".into()
}

/// The full crash message: status plus the stderr tail when there is one.
pub(crate) fn crash_message(
    name: &str,
    status: Option<std::process::ExitStatus>,
    stderr: &StderrTail,
) -> String {
    let status = describe_exit(status);
    match stderr.snapshot() {
        Some(tail) => format!("{name} exited unexpectedly ({status}): {tail}"),
        None => format!("{name} exited unexpectedly ({status})"),
    }
}

pub use claude::ClaudeHarness;
pub use codex::CodexHarness;
pub use omp::OmpHarness;
pub use prime_agent::PrimeAgentHarness;
#[cfg(test)]
mod run_context_tests {
    use super::{InferenceRoute, RunContext, apply_run_context};

    fn configured_env(cmd: &tokio::process::Command, key: &str) -> Option<String> {
        cmd.as_std()
            .get_envs()
            .find(|(name, _)| name.to_string_lossy() == key)
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
    }

    #[test]
    fn applies_exact_session_and_ipc_context() {
        let mut cmd = tokio::process::Command::new("unused");
        apply_run_context(
            &mut cmd,
            Some(&RunContext {
                session_id: "session-123".into(),
                ipc_port: 38117,
                inference: None,
            }),
        );

        assert_eq!(
            configured_env(&cmd, "COMET_SESSION_ID").as_deref(),
            Some("session-123")
        );
        assert_eq!(
            configured_env(&cmd, "COMET_IPC_PORT").as_deref(),
            Some("38117")
        );
    }

    #[test]
    fn applies_loopback_inference_context_without_logging_the_token() {
        let route = InferenceRoute {
            base_url: "http://127.0.0.1:41234".into(),
            token: "local-inference-token".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-5".into(),
        };
        let mut cmd = tokio::process::Command::new("unused");
        apply_run_context(
            &mut cmd,
            Some(&RunContext {
                session_id: "session-123".into(),
                ipc_port: 38117,
                inference: Some(route.clone()),
            }),
        );

        assert_eq!(
            configured_env(&cmd, "ANTHROPIC_BASE_URL").as_deref(),
            Some("http://127.0.0.1:41234")
        );
        assert_eq!(
            configured_env(&cmd, "ANTHROPIC_AUTH_TOKEN").as_deref(),
            Some("local-inference-token")
        );
        assert_eq!(
            configured_env(&cmd, "OMP_AUTH_GATEWAY_URL").as_deref(),
            Some("http://127.0.0.1:41234")
        );
        assert_eq!(
            configured_env(&cmd, "OMP_AUTH_GATEWAY_TOKEN").as_deref(),
            Some("local-inference-token")
        );
        assert_eq!(configured_env(&cmd, "OMP_AUTH_BROKER_URL"), None);
        assert_eq!(configured_env(&cmd, "OMP_AUTH_BROKER_TOKEN"), None);
        assert!(!format!("{route:?}").contains("local-inference-token"));
    }

    #[test]
    fn absent_context_does_not_expose_comet_environment() {
        let mut cmd = tokio::process::Command::new("unused");
        apply_run_context(&mut cmd, None);

        assert_eq!(configured_env(&cmd, "COMET_SESSION_ID"), None);
        assert_eq!(configured_env(&cmd, "COMET_IPC_PORT"), None);
    }
}
