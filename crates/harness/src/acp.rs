//! Installed ACP agents sharing Crew's existing Prime Agent protocol runtime.
//! Native Claude/Codex and OMP keep their dedicated drivers and authentication.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::omp::rpc::{Incoming, RpcClient};
use crate::omp::{AcpProcess, AcpRunOptions, run_acp};
use crate::{Harness, HarnessError, RunControls, StderrTail};
use comet_proto::{
    AgentEvent, HarnessCommand, HarnessId, Model, ModelOption, ModelOptionChoice, ReasoningLevel,
    RunRequest, SteeringMode,
};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct AcpHarness {
    id: HarnessId,
    name: &'static str,
    binary: &'static str,
    override_env: &'static str,
    args: &'static [&'static str],
    executable: Option<PathBuf>,
}

impl AcpHarness {
    pub fn devin() -> Self {
        Self {
            id: HarnessId::Devin,
            name: "Devin",
            binary: "devin",
            override_env: "DEVIN_EXECUTABLE",
            args: &["acp"],
            executable: None,
        }
    }

    pub fn grok() -> Self {
        // Upstream verified these flags avoid a stale shared leader and a
        // launch-time updater that can otherwise leave ACP silently wedged.
        Self {
            id: HarnessId::Grok,
            name: "Grok",
            binary: "grok",
            override_env: "GROK_EXECUTABLE",
            args: &["--no-auto-update", "agent", "--no-leader", "stdio"],
            executable: None,
        }
    }

    pub fn hermes() -> Self {
        Self {
            id: HarnessId::Hermes,
            name: "Hermes",
            binary: "hermes",
            override_env: "HERMES_EXECUTABLE",
            args: &["acp"],
            executable: None,
        }
    }

    pub fn pi() -> Self {
        // Use the installed adapter; never install packages as a side effect
        // of opening a picker or launching a session.
        Self {
            id: HarnessId::Pi,
            name: "Pi",
            binary: "pi-acp",
            override_env: "PI_ACP_EXECUTABLE",
            args: &[],
            executable: None,
        }
    }

    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = Some(executable.into());
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(path) = &self.executable {
            return Ok(path.clone());
        }
        if let Some(path) = std::env::var_os(self.override_env).filter(|path| !path.is_empty()) {
            return Ok(path.into());
        }
        let mut dirs: Vec<_> = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        if let Some(path) = crate::shell_env::login_shell_path() {
            dirs.extend(std::env::split_paths(path));
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            dirs.extend([
                home.join(".local/bin"),
                home.join(".grok/bin"),
                home.join(".hermes/bin"),
                home.join(".npm-global/bin"),
            ]);
        }
        dirs.extend([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ]);
        dirs.extend(crate::node_version_manager_bins());
        dirs.into_iter()
            .map(|dir| dir.join(self.binary))
            .find(|path| is_executable(path))
            .ok_or_else(|| {
                HarnessError::NotInstalled(format!(
                    "{}: install and authenticate {} (or set {})",
                    self.name, self.binary, self.override_env
                ))
            })
    }

    fn command(&self, executable: &Path, cwd: &str) -> Command {
        let mut command = Command::new(executable);
        crate::compose_child_path(&mut command, executable);
        // Clear inherited Crew routing tokens. These agents use their own
        // authenticated CLI, never an OMP provider extension or fallback route.
        crate::apply_run_context(&mut command, None);
        if !cwd.is_empty() {
            command.current_dir(cwd);
        }
        command.kill_on_drop(true);
        command
    }

    fn spawn(
        &self,
        cwd: &str,
        context: Option<&crate::RunContext>,
    ) -> Result<(Child, RpcClient, mpsc::Receiver<Incoming>, StderrTail), HarnessError> {
        let executable = self.resolve_executable()?;
        let mut command = self.command(&executable, cwd);
        crate::apply_run_context(&mut command, context);
        let mut child = command
            .args(self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("ACP child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("ACP child has no stdout".into()))?;
        let tail = StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tail.push(&line);
                }
            });
        }
        let (client, incoming) = RpcClient::new(stdin, stdout);
        Ok((child, client, incoming, tail))
    }

    async fn discover(
        &self,
        cwd: &str,
        wait_for_commands: bool,
    ) -> Result<(Value, Vec<HarnessCommand>), HarnessError> {
        let cwd = if cwd.is_empty() {
            std::env::current_dir()?.to_string_lossy().into_owned()
        } else {
            cwd.to_string()
        };
        let (mut child, client, mut incoming, _) = self.spawn(&cwd, None)?;
        let mut commands = Vec::new();
        let probe = async {
            let initialized = discovery_request(
                &client,
                &mut incoming,
                "initialize",
                json!({
                    "protocolVersion": 1, "clientCapabilities": {},
                    "clientInfo": {"name": "crew", "version": env!("CARGO_PKG_VERSION")}
                }),
                &mut commands,
            )
            .await?;
            if initialized.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
                return Err(HarnessError::Protocol(
                    "ACP protocol version mismatch".into(),
                ));
            }
            let state = discovery_request(
                &client,
                &mut incoming,
                "session/new",
                json!({"cwd": cwd, "mcpServers": []}),
                &mut commands,
            )
            .await?;
            if wait_for_commands && commands.is_empty() {
                // ACP advertises commands asynchronously after session/new.
                // Match upstream's bounded grace, not a timing-dependent empty catalog.
                let deadline = tokio::time::sleep(Duration::from_secs(2));
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        _ = &mut deadline => break,
                        item = incoming.recv() => match item {
                            Some(Incoming::Eof) | None => break,
                            Some(item) => discovery_incoming(&client, item, &mut commands),
                        }
                    }
                    if !commands.is_empty() {
                        break;
                    }
                }
            }
            Ok(state)
        };
        let result = tokio::time::timeout(DISCOVERY_TIMEOUT, probe)
            .await
            .map_err(|_| HarnessError::Protocol(format!("{} ACP catalog timed out", self.name)))
            .and_then(|result| result);
        let _ = child.kill().await;
        result.map(|state| (state, commands))
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

async fn discovery_request(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    method: &str,
    params: Value,
    commands: &mut Vec<HarnessCommand>,
) -> Result<Value, HarnessError> {
    let request = client.request(method, params);
    tokio::pin!(request);
    loop {
        tokio::select! {
            biased;
            result = &mut request => {
                while let Ok(item) = incoming.try_recv() { discovery_incoming(client, item, commands); }
                return result;
            }
            item = incoming.recv() => match item {
                Some(Incoming::Eof) | None => return Err(HarnessError::Protocol("ACP catalog stream closed".into())),
                Some(item) => discovery_incoming(client, item, commands),
            }
        }
    }
}

fn discovery_incoming(client: &RpcClient, incoming: Incoming, commands: &mut Vec<HarnessCommand>) {
    match incoming {
        Incoming::Notification { method, params }
            if method == "session/update"
                && params["update"]["sessionUpdate"] == "available_commands_update" =>
        {
            *commands = parse_commands(&params["update"]);
        }
        Incoming::Request { id, method, .. } if method == "session/request_permission" => {
            // Discovery is read-only and has no interactive approval channel.
            client.respond(&id, json!({"outcome": {"outcome": "cancelled"}}));
        }
        Incoming::Request { id, .. } => {
            client.respond_error(&id, -32601, "unsupported ACP catalog method")
        }
        _ => {}
    }
}

fn parse_commands(update: &Value) -> Vec<HarnessCommand> {
    update
        .get("availableCommands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|command| {
            let name = command
                .get("name")?
                .as_str()?
                .trim()
                .trim_start_matches('/');
            if name.is_empty() {
                return None;
            }
            Some(HarnessCommand {
                name: name.into(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                input_hint: command
                    .pointer("/input/hint")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                aliases: Vec::new(),
                subcommands: Vec::new(),
                source: Some("agent".into()),
            })
        })
        .collect()
}

pub(crate) fn config_option<'a>(
    state: &'a Value,
    category: &str,
    ids: &[&str],
) -> Option<&'a Value> {
    state
        .get("configOptions")?
        .as_array()?
        .iter()
        .find(|option| {
            (!category.is_empty()
                && option.get("category").and_then(Value::as_str) == Some(category))
                || option
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| ids.contains(&id))
        })
}

fn choices(option: &Value) -> Vec<ModelOptionChoice> {
    let mut choices = Vec::new();
    for entry in option
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let entries = entry
            .get("options")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_else(|| std::slice::from_ref(entry));
        for entry in entries {
            if let Some(id) = entry.get("value").and_then(Value::as_str) {
                choices.push(ModelOptionChoice {
                    id: id.into(),
                    label: entry
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .into(),
                });
            }
        }
    }
    choices
}

pub(crate) fn models_from_session(state: &Value) -> Vec<Model> {
    let reasoning_levels: Vec<_> = config_option(
        state,
        "thought_level",
        &["thinking", "reasoning", "thought_level"],
    )
    .map(choices)
    .unwrap_or_default()
    .into_iter()
    .filter_map(|choice| serde_json::from_value(Value::String(choice.id)).ok())
    .collect();
    let options: Vec<_> = state
        .get("configOptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| {
            let category = option.get("category").and_then(Value::as_str).unwrap_or("");
            let id = option.get("id")?.as_str()?;
            if matches!(category, "model" | "thought_level" | "mode")
                || matches!(
                    id,
                    "model" | "thinking" | "reasoning" | "thought_level" | "mode"
                )
            {
                return None;
            }
            let choices = choices(option);
            let default_choice = option.get("currentValue")?.as_str()?.to_string();
            if choices.is_empty() {
                return None;
            }
            Some(ModelOption {
                id: id.into(),
                label: option
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .into(),
                choices,
                default_choice,
            })
        })
        .collect();
    let make = |id: String, label: String, description: Option<String>| Model {
        id,
        label,
        description,
        reasoning_levels: reasoning_levels.clone(),
        options: options.clone(),
    };
    if let Some(option) = config_option(state, "model", &["model"]) {
        let models: Vec<_> = choices(option)
            .into_iter()
            .map(|choice| make(choice.id, choice.label, None))
            .collect();
        if !models.is_empty() {
            return models;
        }
    }
    state
        .pointer("/models/availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model.get("modelId")?.as_str()?;
            Some(make(
                id.into(),
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .into(),
                model
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ))
        })
        .collect()
}

#[async_trait]
impl Harness for AcpHarness {
    fn id(&self) -> HarnessId {
        self.id
    }
    fn display_name(&self) -> &str {
        self.name
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        // Effort is a live model capability, not an assumption about a CLI
        // version. An empty advertised ladder must not gain picker defaults.
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        if self.id == HarnessId::Devin {
            // Devin's session/new contains a stale bundled catalog. The CLI
            // waits for the account catalog and returns exact effort variants.
            let executable = self.resolve_executable()?;
            let output = tokio::time::timeout(
                DISCOVERY_TIMEOUT,
                self.command(&executable, "")
                    .args(["models", "list", "--format", "json"])
                    .stdin(Stdio::null())
                    .output(),
            )
            .await
            .map_err(|_| HarnessError::Protocol("Devin model discovery timed out".into()))??;
            if !output.status.success() {
                return Err(HarnessError::Protocol(format!(
                    "Devin model discovery failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            return devin_models(&output.stdout);
        }
        let (state, _) = self.discover("", false).await?;
        let mut models = models_from_session(&state);
        if models.is_empty() {
            models.push(Model {
                id: "default".into(),
                label: format!("{} configured model", self.name),
                description: Some("Uses this agent CLI's current model and authentication".into()),
                reasoning_levels: Vec::new(),
                options: Vec::new(),
            });
        }
        Ok(models)
    }

    async fn commands(&self, cwd: &str) -> Result<Vec<HarnessCommand>, HarnessError> {
        self.discover(cwd, true).await.map(|(_, commands)| commands)
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        if request.sandbox == comet_proto::SandboxLevel::ReadOnly {
            return Err(HarnessError::Protocol(format!(
                "{} does not advertise an enforceable read-only sandbox through ACP",
                self.name
            )));
        }
        if request.agent_account_id.is_some()
            || controls
                .context
                .as_ref()
                .is_some_and(|context| context.inference.is_some() || context.fork_from.is_some())
        {
            return Err(HarnessError::Protocol(format!(
                "{} uses its own CLI authentication and ACP resume; Crew account routing and native forks are not supported",
                self.name
            )));
        }
        let (child, client, incoming, tail) =
            self.spawn(&request.cwd, controls.context.as_ref())?;
        let (events, receiver) = mpsc::channel(256);
        let id = self.id;
        let name = self.name;
        tokio::spawn(async move {
            if let Err(error) = run_acp(
                AcpProcess::new(child, client, incoming, tail),
                request,
                controls,
                events.clone(),
                Duration::from_secs(3),
                AcpRunOptions {
                    harness: id,
                    process_label: name,
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

fn devin_models(bytes: &[u8]) -> Result<Vec<Model>, HarnessError> {
    #[derive(Deserialize)]
    struct Catalog {
        families: Vec<Family>,
    }
    #[derive(Deserialize)]
    struct Family {
        variants: Vec<Variant>,
    }
    #[derive(Deserialize)]
    struct Variant {
        model_uid: String,
        label: String,
        cost_summary: Option<String>,
    }
    let catalog: Catalog = serde_json::from_slice(bytes)
        .map_err(|error| HarnessError::Protocol(format!("Invalid Devin model catalog: {error}")))?;
    let mut models = Vec::new();
    for variant in catalog
        .families
        .into_iter()
        .flat_map(|family| family.variants)
    {
        if variant.model_uid.trim().is_empty() || variant.label.trim().is_empty() {
            return Err(HarnessError::Protocol(
                "Empty Devin model id or label".into(),
            ));
        }
        if !models
            .iter()
            .any(|model: &Model| model.id == variant.model_uid)
        {
            models.push(Model {
                id: variant.model_uid,
                label: variant.label,
                description: variant.cost_summary,
                reasoning_levels: Vec::new(),
                options: Vec::new(),
            });
        }
    }
    if models.is_empty() {
        return Err(HarnessError::Protocol(
            "Devin returned an empty model catalog".into(),
        ));
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_model_options_take_precedence_over_legacy_matrix() {
        let state = json!({"models": {"availableModels": [{"modelId":"stale"}]}, "configOptions": [
            {"id":"selected_model", "category":"model", "options":[{"group":"Provider", "options":[{"value":"current","name":"Current model"}]}]},
            {"id":"effort", "category":"thought_level", "options":[{"value":"high","name":"High"}]}
        ]});
        let models = models_from_session(&state);
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["current"]
        );
        assert_eq!(models[0].reasoning_levels, [ReasoningLevel::High]);
        assert_eq!(
            models_from_session(
                &json!({"models":{"availableModels":[{"modelId":"legacy","name":"Legacy model"}]}})
            )[0]
            .id,
            "legacy"
        );
    }

    #[test]
    fn devin_catalog_keeps_exact_variants_and_rejects_empty_catalogs() {
        let models = devin_models(br#"{"families":[{"variants":[{"model_uid":"astra-high","label":"Astra High"},{"model_uid":"astra-high","label":"Duplicate"},{"model_uid":"astra-medium","label":"Astra Medium"}]}]}"#).unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["astra-high", "astra-medium"]
        );
        assert!(devin_models(br#"{"families":[]}"#).is_err());
    }
}
