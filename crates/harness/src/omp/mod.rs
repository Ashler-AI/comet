//! OMP harness adapter over Agent Client Protocol (ACP) stdio.
//!
//! Comet owns execution through `omp acp`; OMP's read-only `models --json`
//! command supplies the selectable provider/model catalog.

pub(crate) mod rpc;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt as _;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode, ToolCall,
    UserInputQuestion,
};

use crate::{Harness, HarnessError, RunControls};
use rpc::{Incoming, RpcClient};

const ACP_PROTOCOL_VERSION: i64 = 1;
const SCAFFOLD_PROFILE: &str = "scaffold-host";
const AUTH_BROKER_URL_ENV: &str = "OMP_AUTH_BROKER_URL";
const AUTH_BROKER_TOKEN_ENV: &str = "OMP_AUTH_BROKER_TOKEN";
const AUTH_BROKER_TOKEN_FILE_ENV: &str = "OMP_AUTH_BROKER_TOKEN_FILE";
const REASONING_LEVELS: &[ReasoningLevel] = &[
    ReasoningLevel::Minimal,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
];

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

fn propagate_auth_broker_environment(command: &mut Command, scaffold_host: bool) {
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
    if let Some(url) = std::env::var_os(AUTH_BROKER_URL_ENV).filter(|value| !value.is_empty()) {
        command.env(AUTH_BROKER_URL_ENV, url);
    }
    // Local controllers may inherit a workstation broker token. A scoped host
    // must never accept that long-lived credential and can use only its
    // single-use token file projection.
    if !scaffold_host {
        if let Some(token) =
            std::env::var_os(AUTH_BROKER_TOKEN_ENV).filter(|value| !value.is_empty())
        {
            command.env(AUTH_BROKER_TOKEN_ENV, token);
            return;
        }
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
}

impl Default for OmpHarness {
    fn default() -> Self {
        Self {
            executable: None,
            scaffold_host: false,
            interrupt_grace: Duration::from_secs(3),
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

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        self.executable.clone().or_else(resolve_omp_executable).ok_or_else(|| {
            HarnessError::NotInstalled("omp (searched OMP_EXECUTABLE, PATH, login-shell PATH, ~/.local/bin, /usr/local/bin, and Node manager bins)".into())
        })
    }

    fn base_command(&self, executable: &Path, cwd: &str) -> Command {
        let mut command = Command::new(executable);
        crate::compose_child_path(&mut command, executable);
        propagate_auth_broker_environment(&mut command, self.scaffold_host);
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

    fn command(&self, executable: &Path, cwd: &str) -> Command {
        let mut command = self.base_command(executable, cwd);
        command.arg("acp");
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
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_executable()?;
        Ok(vec![Model {
            id: "default".into(),
            label: "OMP default".into(),
            description: Some("Provider and model selected by the OMP profile".into()),
            reasoning_levels: REASONING_LEVELS.to_vec(),
            options: vec![],
        }])
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let executable = self.resolve_executable()?;
        let mut child = self
            .command(&executable, &request.cwd)
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
                child,
                client,
                incoming,
                request,
                controls,
                events.clone(),
                interrupt_grace,
                stderr_tail,
                AcpRunOptions {
                    harness: HarnessId::Omp,
                    process_label: "OMP ACP",
                    preloaded_session_id: None,
                    configure_session: true,
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

pub(crate) struct AcpRunOptions {
    pub harness: HarnessId,
    pub process_label: &'static str,
    pub preloaded_session_id: Option<String>,
    pub configure_session: bool,
}

pub(crate) async fn run_acp(
    mut child: Child,
    client: RpcClient,
    mut incoming: mpsc::Receiver<Incoming>,
    request: RunRequest,
    controls: RunControls,
    events: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    interrupt_grace: Duration,
    stderr_tail: crate::StderrTail,
    options: AcpRunOptions,
) -> Result<(), HarnessError> {
    let initialized = client
        .request(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {},
                "clientInfo": { "name": "ashler-comet", "version": env!("CARGO_PKG_VERSION") }
            }),
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
            let state = client
                .request(
                    "session/new",
                    json!({ "cwd": request.cwd, "mcpServers": [] }),
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
            let state = client
                .request(
                    method,
                    json!({ "sessionId": session_id, "cwd": request.cwd, "mcpServers": [] }),
                )
                .await?;
            (session_id.to_string(), session_id.to_string(), state)
        } else {
            let state = client
                .request(
                    "session/new",
                    json!({ "cwd": request.cwd, "mcpServers": [] }),
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
        configure_session(&client, &session_id, &session_state, &request).await?;
    }
    events
        .send(Ok(AgentEvent::SessionStarted {
            harness: options.harness,
            model: request.model.clone().unwrap_or_else(|| "default".into()),
            tools: vec![],
            cwd: request.cwd.clone(),
            session_id: reported_session_id.clone(),
            assistant_message_id: format!("acp-{session_id}"),
        }))
        .await
        .ok();

    let mut prompt_blocks = Vec::with_capacity(request.attachments.len() + 1);
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
        prompt_blocks.push(json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            "mimeType": mime_type,
            "uri": format!("file://{attachment}")
        }));
    }
    prompt_blocks.push(json!({ "type": "text", "text": request.prompt }));
    let prompt = client.request(
        "session/prompt",
        json!({ "sessionId": session_id, "prompt": prompt_blocks }),
    );
    tokio::pin!(prompt);
    loop {
        tokio::select! {
            response = &mut prompt => {
                let response = response?;
                // The reader resolves responses directly and forwards notifications
                // through a separate channel. A fast agent may therefore make the
                // terminal response ready while earlier update frames are still
                // queued. Drain that ordered queue before emitting Done.
                while let Ok(incoming) = incoming.try_recv() {
                    match incoming {
                        Incoming::Notification { method, params } if method == "session/update" => {
                            if let Some(event) = normalize_update(&params) { events.send(Ok(event)).await.ok(); }
                        }
                        Incoming::Request { id, .. } => client.respond_error(&id, -32601, "ACP request arrived after prompt completion"),
                        Incoming::Eof | Incoming::Notification { .. } => {}
                    }
                }
                if let Some(usage) = response.get("usage") {
                    events.send(Ok(AgentEvent::Usage {
                        input_tokens: usage.get("inputTokens").and_then(Value::as_u64).unwrap_or(0),
                        output_tokens: usage.get("outputTokens").and_then(Value::as_u64).unwrap_or(0),
                    })).await.ok();
                }
                let reason = response.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn");
                let status = if reason == "cancelled" { DoneStatus::Interrupted } else { DoneStatus::Completed };
                events.send(Ok(AgentEvent::Done { status, result: None, error: None, session_id: Some(reported_session_id.clone()) })).await.ok();
                return Ok(());
            }
            _ = controls.interrupt.cancelled() => {
                client.notify("session/cancel", Some(json!({ "sessionId": session_id })));
                let _ = tokio::time::timeout(interrupt_grace, child.wait()).await;
                let _ = child.kill().await;
                events.send(Ok(AgentEvent::Done { status: DoneStatus::Interrupted, result: None, error: None, session_id: Some(reported_session_id.clone()) })).await.ok();
                return Ok(());
            }
            incoming = incoming.recv() => match incoming {
                Some(Incoming::Notification { method, params }) if method == "session/update" => {
                    if let Some(event) = normalize_update(&params) { events.send(Ok(event)).await.ok(); }
                }
                Some(Incoming::Request { id, method, params }) if method == "session/request_permission" => {
                    handle_permission_request(
                        id,
                        params,
                        client.clone(),
                        request.auto_approve,
                        &controls.request_input,
                    );
                }
                Some(Incoming::Request { id, .. }) => client.respond_error(&id, -32601, "unsupported ACP client method"),
                Some(Incoming::Eof) | None => {
                    return Err(HarnessError::Protocol(crate::crash_message(options.process_label, child.try_wait().ok().flatten(), &stderr_tail)));
                }
                _ => {}
            }
        }
    }
}

type RequestInputFn = Box<
    dyn Fn(
            Vec<UserInputQuestion>,
        ) -> tokio::sync::oneshot::Receiver<Vec<comet_proto::UserInputAnswer>>
        + Send
        + Sync,
>;

fn handle_permission_request(
    id: Value,
    params: Value,
    client: RpcClient,
    auto_approve: bool,
    request_input: &RequestInputFn,
) {
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if auto_approve {
        let option_id = options.iter().find_map(|option| {
            let kind = option
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let option_id = option
                .get("optionId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            (kind == "allow_once" || option_id.contains("allow_once"))
                .then_some(option_id)
                .filter(|value| !value.is_empty())
        });
        match option_id {
            Some(option_id) => client.respond(
                &id,
                json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
            ),
            None => client.respond(&id, json!({ "outcome": { "outcome": "cancelled" } })),
        }
        return;
    }
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

fn content_text(content: &Value) -> Option<&str> {
    content.get("text").and_then(Value::as_str)
}

fn normalize_update(params: &Value) -> Option<AgentEvent> {
    let update = params.get("update")?;
    match update.get("sessionUpdate")?.as_str()? {
        "agent_message_chunk" => Some(AgentEvent::TextDelta {
            text: content_text(update.get("content")?)?.into(),
        }),
        "agent_thought_chunk" => Some(AgentEvent::ReasoningDelta {
            text: content_text(update.get("content")?)?.into(),
        }),
        "tool_call" => Some(AgentEvent::ToolCall {
            id: update.get("toolCallId")?.as_str()?.into(),
            call: ToolCall::Unknown {
                name: update
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("OMP tool")
                    .into(),
                input: update.get("rawInput").cloned(),
            },
        }),
        "tool_call_update" => {
            let status = update.get("status").and_then(Value::as_str).unwrap_or("");
            matches!(status, "completed" | "failed").then(|| AgentEvent::ToolResult {
                id: update
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("omp-tool")
                    .into(),
                is_error: status == "failed",
            })
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
        let command = harness.command(Path::new("/usr/local/bin/omp"), "/workspace");
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.first().map(String::as_str), Some("acp"));
        for required in [
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
        let command =
            OmpHarness::scaffold_host().command(Path::new("/usr/local/bin/omp"), "/workspace");
        let process = command.as_std();
        let args: Vec<_> = process
            .get_args()
            .map(|value| value.to_string_lossy())
            .collect();
        assert_eq!(
            args,
            [
                "acp",
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
        let insecure =
            OmpHarness::scaffold_host().command(Path::new("/usr/local/bin/omp"), "/workspace");
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
    fn normalizes_text_reasoning_and_tool_updates() {
        assert_eq!(
            normalize_update(
                &json!({"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}})
            ),
            Some(AgentEvent::TextDelta { text: "hi".into() })
        );
        assert_eq!(
            normalize_update(
                &json!({"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"why"}}})
            ),
            Some(AgentEvent::ReasoningDelta { text: "why".into() })
        );
        assert_eq!(
            normalize_update(
                &json!({"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"failed"}})
            ),
            Some(AgentEvent::ToolResult {
                id: "t1".into(),
                is_error: true
            })
        );
    }
}
