//! Codex's app-server exposes skills, not the TUI's deprecated custom prompts.
//! Explicit skill inputs preserve native skill loading and approval checks.

use std::process::Stdio;
use std::time::Duration;

use comet_proto::HarnessCommand;
use serde_json::{Value, json};
use tokio::process::Command;

use super::rpc::{Incoming, RpcClient};
use super::{CodexHarness, shutdown_child};
use crate::HarnessError;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) async fn initialize(client: &RpcClient) -> Result<(), HarnessError> {
    client
        .request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "comet-native",
                    "title": "Crew",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": { "experimentalApi": true },
            }),
        )
        .await?;
    client.notify("initialized", None);
    Ok(())
}

async fn skills(client: &RpcClient, cwd: &str) -> Result<Vec<Value>, HarnessError> {
    let cwds = if cwd.is_empty() {
        Vec::new()
    } else {
        vec![cwd]
    };
    let mut response = client
        .request("skills/list", json!({ "cwds": cwds, "forceReload": true }))
        .await?;
    let entries = response["data"]
        .as_array_mut()
        .ok_or_else(|| HarnessError::Protocol("skills/list: missing data".into()))?;
    let mut skills = Vec::new();
    for entry in entries {
        let catalog = entry["skills"]
            .as_array_mut()
            .ok_or_else(|| HarnessError::Protocol("skills/list: missing skills".into()))?;
        for skill in catalog.drain(..) {
            let Some(name) = skill["name"].as_str() else {
                continue;
            };
            if skill["enabled"].as_bool() == Some(true)
                && !name.is_empty()
                && !name.starts_with('/')
                && !name.chars().any(char::is_whitespace)
                && skill["path"].as_str().is_some_and(|path| !path.is_empty())
            {
                skills.push(skill);
            }
        }
    }
    skills.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    skills.dedup_by(|a, b| a["name"] == b["name"]);
    Ok(skills)
}

pub(super) async fn discover(
    harness: &CodexHarness,
    cwd: &str,
) -> Result<Vec<HarnessCommand>, HarnessError> {
    let exe = harness.resolve_executable()?;
    let mut command = Command::new(&exe);
    command.arg("app-server");
    crate::compose_child_path(&mut command, &exe);
    if !cwd.is_empty() {
        command.current_dir(cwd);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(error)
            }
        })?;
    let (client, mut incoming) = RpcClient::new(
        child.stdin.take().expect("piped stdin"),
        child.stdout.take().expect("piped stdout"),
    );
    let discovery = async {
        initialize(&client).await?;
        skills(&client, cwd).await
    };
    let response = tokio::time::timeout(DISCOVERY_TIMEOUT, async {
        tokio::pin!(discovery);
        loop {
            tokio::select! {
                result = &mut discovery => break result,
                message = incoming.recv() => match message {
                    Some(Incoming::Request { id, .. }) => {
                        // Discovery never grants provider permissions or runs inference.
                        client.respond_error(&id, -32601, "command discovery cannot approve requests");
                    }
                    Some(Incoming::Notification { .. }) => {}
                    Some(Incoming::Eof) | None => break Err(HarnessError::Protocol(
                        "Codex command discovery exited before responding".into(),
                    )),
                }
            }
        }
    })
    .await;
    shutdown_child(&mut child, harness.kill_grace).await;
    let skills = response
        .map_err(|_| HarnessError::Protocol("Codex command discovery timed out".into()))??;
    let mut commands: Vec<_> = skills
        .into_iter()
        .filter(|skill| skill["name"] != "review")
        .map(|skill| HarnessCommand {
            name: skill["name"].as_str().unwrap().into(),
            description: skill["interface"]["shortDescription"]
                .as_str()
                .or_else(|| skill["shortDescription"].as_str())
                .or_else(|| skill["description"].as_str())
                .unwrap_or_default()
                .into(),
            input_hint: Some("[instructions]".into()),
            aliases: Vec::new(),
            subcommands: Vec::new(),
            source: Some("skill".into()),
        })
        .collect();
    commands.push(HarnessCommand {
        name: "review".into(),
        description: "Review uncommitted changes, or provide custom review instructions".into(),
        input_hint: Some("[instructions]".into()),
        aliases: Vec::new(),
        subcommands: Vec::new(),
        source: Some("builtin".into()),
    });
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(commands)
}

pub(super) fn review_target(text: &str) -> Option<Value> {
    let (name, instructions) = split_command(text)?;
    (name == "review").then(|| {
        if instructions.is_empty() {
            json!({ "type": "uncommittedChanges" })
        } else {
            json!({ "type": "custom", "instructions": instructions })
        }
    })
}

fn split_command(text: &str) -> Option<(&str, &str)> {
    let command = text.strip_prefix('/')?;
    Some(
        command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(name, arguments)| {
                (name, arguments.trim_start())
            }),
    )
}

pub(super) async fn input(
    client: &RpcClient,
    cwd: &str,
    text: &str,
) -> Result<Value, HarnessError> {
    let Some((name, arguments)) = split_command(text) else {
        return Ok(json!([{ "type": "text", "text": text }]));
    };
    if name.is_empty() || name.contains('/') {
        return Ok(json!([{ "type": "text", "text": text }]));
    }
    let catalog = tokio::time::timeout(DISCOVERY_TIMEOUT, skills(client, cwd))
        .await
        .map_err(|_| HarnessError::Protocol("Codex skill lookup timed out".into()))??;
    let Some(skill) = catalog.iter().find(|skill| skill["name"] == name) else {
        return Ok(json!([{ "type": "text", "text": text }]));
    };
    Ok(json!([
        { "type": "skill", "name": name, "path": skill["path"] },
        { "type": "text", "text": arguments },
    ]))
}
