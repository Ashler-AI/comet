//! Prime Agent harness over its persistent Agent Client Protocol (ACP) mode.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use comet_proto::{
    AgentEvent, HarnessCommand, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
};

use crate::omp::rpc::RpcClient;
use crate::omp::{AcpProcess, AcpRunOptions, run_acp};
use crate::{Harness, HarnessError, RunControls};

const REASONING_LEVELS: &[ReasoningLevel] = &[
    ReasoningLevel::Minimal,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
];
const PRIME_AGENT_AUTH_GATEWAY: &str = include_str!("prime_agent_auth_gateway.ts");

fn resolve_prime_agent_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PRIME_AGENT_EXECUTABLE").filter(|value| !value.is_empty())
    {
        return Some(PathBuf::from(path));
    }
    let executable = if cfg!(windows) {
        "prime-agent.exe"
    } else {
        "prime-agent"
    };
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

fn prime_config_dir() -> PathBuf {
    std::env::var_os("PRIME_AGENT_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".prime/agent"))
        })
        .unwrap_or_else(|| PathBuf::from(".prime/agent"))
}

fn bundled_auth_gateway_extension() -> Result<Option<PathBuf>, HarnessError> {
    let discovered = prime_config_dir().join("extensions/omp-auth-gateway.ts");
    if discovered.is_file() {
        return Ok(None);
    }
    let directory = prime_config_dir().join("comet-runtime");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("prime-agent-auth-gateway.ts");
    let current = std::fs::read(&path).ok();
    if current.as_deref() != Some(PRIME_AGENT_AUTH_GATEWAY.as_bytes()) {
        std::fs::write(&path, PRIME_AGENT_AUTH_GATEWAY)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(Some(path))
}

fn reasoning_arg(level: ReasoningLevel) -> &'static str {
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

fn models_from_output(stdout: &[u8]) -> Vec<Model> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            if columns.len() < 6 || columns[0] == "provider" {
                return None;
            }
            let model = columns[1];
            let id = format!("{}/{model}", columns[0]);
            if !crate::is_curated_comet_model(&id) {
                return None;
            }
            Some(Model {
                id,
                label: model.to_string(),
                description: Some(format!(
                    "{} context · {} max output · Listed in Prime Agent's catalog; run availability is not verified; authorization is not verified",
                    columns[2], columns[3]
                )),
                reasoning_levels: REASONING_LEVELS.to_vec(),
                options: Vec::new(),
            })
        })
        .collect()
}

fn commands_from_response(value: &Value) -> Vec<HarnessCommand> {
    let mut commands: Vec<_> = value
        .pointer("/data/commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|command| {
            let name = command.get("name")?.as_str()?.trim();
            if name.is_empty() {
                return None;
            }
            Some(HarnessCommand {
                name: name.trim_start_matches('/').to_string(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                input_hint: None,
            })
        })
        .collect();
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands.dedup_by(|a, b| a.name == b.name);
    commands
}

pub struct PrimeAgentHarness {
    executable: Option<PathBuf>,
    interrupt_grace: Duration,
}

impl Default for PrimeAgentHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(3),
        }
    }
}

impl PrimeAgentHarness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = Some(executable.into());
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        self.executable
            .clone()
            .or_else(resolve_prime_agent_executable)
            .ok_or_else(|| {
                HarnessError::NotInstalled(
                    "prime-agent (searched PRIME_AGENT_EXECUTABLE, PATH, login-shell PATH, ~/.local/bin, /usr/local/bin, and Node manager bins)".into(),
                )
            })
    }

    fn command(&self, executable: &Path, cwd: &str) -> Command {
        let mut command = Command::new(executable);
        command.args(["--session-store", "prime"]);
        crate::compose_child_path(&mut command, executable);
        if !cwd.is_empty() {
            command.current_dir(cwd);
        }
        command
    }

    fn new_session_dir() -> Result<PathBuf, HarnessError> {
        let path = prime_config_dir()
            .join("comet-sessions")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }
}

#[async_trait]
impl Harness for PrimeAgentHarness {
    fn id(&self) -> HarnessId {
        HarnessId::PrimeAgent
    }

    fn display_name(&self) -> &str {
        "Prime Agent"
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        REASONING_LEVELS
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let executable = self.resolve_executable()?;
        let output = self
            .command(&executable, "")
            .args(["model", "list"])
            .stdin(Stdio::null())
            .output()
            .await?;
        if !output.status.success() {
            let tail = crate::StderrTail::default();
            for line in String::from_utf8_lossy(&output.stderr).lines() {
                tail.push(line);
            }
            return Err(HarnessError::Protocol(crate::crash_message(
                "Prime Agent model catalog",
                Some(output.status),
                &tail,
            )));
        }
        let catalog = if output.stdout.is_empty() {
            output.stderr.as_slice()
        } else {
            output.stdout.as_slice()
        };
        let models = models_from_output(catalog);
        if models.is_empty() {
            return Err(HarnessError::Protocol(
                "Prime Agent model catalog was empty".into(),
            ));
        }
        Ok(models)
    }

    async fn commands(&self, cwd: &str) -> Result<Vec<HarnessCommand>, HarnessError> {
        let executable = self.resolve_executable()?;
        let mut child = self
            .command(&executable, cwd)
            .args(["--mode", "rpc", "--no-session"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("Prime Agent RPC child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("Prime Agent RPC child has no stdout".into()))?;
        stdin
            .write_all(b"{\"id\":\"comet-commands\",\"type\":\"get_commands\"}\n")
            .await?;
        stdin.flush().await?;
        let mut lines = BufReader::new(stdout).lines();
        let response = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await? {
                let value: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                if value.get("type").and_then(Value::as_str) == Some("response")
                    && value.get("command").and_then(Value::as_str) == Some("get_commands")
                {
                    return Ok::<_, std::io::Error>(Some(value));
                }
            }
            Ok(None)
        })
        .await
        .map_err(|_| HarnessError::Protocol("Prime Agent command catalog timed out".into()))??;
        let _ = child.kill().await;
        let response = response.ok_or_else(|| {
            HarnessError::Protocol("Prime Agent command catalog closed without a response".into())
        })?;
        if response.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(HarnessError::Protocol(
                response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Prime Agent command catalog request failed")
                    .to_string(),
            ));
        }
        Ok(commands_from_response(&response))
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let executable = self.resolve_executable()?;
        let preloaded_session_id = request.resume.clone();
        let mut command = self.command(&executable, &request.cwd);
        if controls
            .context
            .as_ref()
            .and_then(|context| context.inference.as_ref())
            .is_some()
        {
            command.env("PRIME_AGENT_MANAGE_AUTH_ROUTER", "0");
            if let Some(extension) = bundled_auth_gateway_extension()? {
                command.arg("--extension").arg(extension);
            }
        }
        crate::apply_run_context(&mut command, controls.context.as_ref());
        let reported_session_dir = if let Some(resume) = request.resume.as_deref() {
            command.args(["--resume", resume]);
            None
        } else {
            let session_dir = Self::new_session_dir()?;
            command.arg("--session-dir").arg(&session_dir);
            Some(session_dir)
        };
        if let Some(model) = request.model.as_deref().filter(|model| *model != "default") {
            command.args(["--model", model]);
        }
        if let Some(reasoning) = request.reasoning {
            command.args(["--thinking", reasoning_arg(reasoning)]);
        }
        command
            .args(["--dangerously-skip-permissions", "--mode", "acp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(executable.display().to_string())
            } else {
                HarnessError::Io(error)
            }
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("Prime Agent ACP child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("Prime Agent ACP child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
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
                    harness: HarnessId::PrimeAgent,
                    process_label: "Prime Agent ACP",
                    preloaded_session_id,
                    reported_session_dir,
                    configure_session: false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_turn_boundary_queueing() {
        let harness = PrimeAgentHarness::new();

        assert!(harness.supports_steering());
        assert_eq!(harness.steering_mode(), SteeringMode::TurnBoundary);
    }

    #[test]
    fn parses_prime_model_catalog() {
        let models = models_from_output(
            b"provider model context max-out thinking images\n\
openai-codex gpt-5.6-sol 1.0M 262.1K yes yes\n\
prime-inference openai/gpt-5.6-sol-pro 1.0M 128K yes yes\n\
openai-codex gpt-5.5 1.0M 262.1K yes yes\n\
openai-codex gpt-5.60-future 1.0M 262.1K yes yes\n\
anthropic claude-opus-5 200K 64K yes yes\n\
prime-inference anthropic/claude-fable-5 1.0M 128K yes yes\n\
prime-inference moonshotai/kimi-k3 262.1K 262.1K yes no\n\
prime-inference x-ai/grok-4.20 2.0M 65.5K yes no\n\
prime-inference x-ai/grok-4.20-multi-agent 2.0M 65.5K yes no\n\
prime-inference z-ai/glm-5.2 1.0M 262.1K yes no\n",
        );
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "openai-codex/gpt-5.6-sol",
                "prime-inference/openai/gpt-5.6-sol-pro",
                "anthropic/claude-opus-5",
                "prime-inference/anthropic/claude-fable-5",
                "prime-inference/moonshotai/kimi-k3",
                "prime-inference/x-ai/grok-4.20",
                "prime-inference/x-ai/grok-4.20-multi-agent",
            ]
        );
        assert_eq!(models[0].reasoning_levels, REASONING_LEVELS);
        assert!(
            models[0]
                .description
                .as_deref()
                .unwrap()
                .contains("run availability is not verified")
        );
        assert!(
            models[0]
                .description
                .as_deref()
                .unwrap()
                .contains("authorization is not verified")
        );
    }

    #[test]
    fn bundles_a_credential_free_loopback_gateway_adapter() {
        assert!(PRIME_AGENT_AUTH_GATEWAY.contains("OMP_AUTH_GATEWAY_TOKEN"));
        assert!(PRIME_AGENT_AUTH_GATEWAY.contains("http://127.0.0.1:4000"));
        assert!(!PRIME_AGENT_AUTH_GATEWAY.contains("sk-"));
        assert!(!PRIME_AGENT_AUTH_GATEWAY.contains("refreshToken"));
    }

    #[test]
    fn parses_and_deduplicates_prime_commands() {
        let commands = commands_from_response(&serde_json::json!({
            "data": {"commands": [
                {"name": "skill:review", "description": "Review"},
                {"name": "/skill:review", "description": "Duplicate"},
                {"name": "session_name", "description": "Rename"}
            ]}
        }));
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].name, "session_name");
        assert_eq!(commands[1].name, "skill:review");
    }
}
