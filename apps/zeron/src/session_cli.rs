use anyhow::{Context, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Read, path::{Path, PathBuf}};

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    Current,
    Read { chat_id: String },
    Search {
        #[arg(long, required_unless_present="source_url")] query: Option<String>,
        #[arg(long, required_unless_present="query")] source_url: Option<String>,
        #[arg(long)] owner_id: Option<String>,
        #[arg(long, value_parser=clap::value_parser!(u8).range(1..=50))] limit: Option<u8>,
        #[arg(long)] cursor: Option<String>,
    },
    Fork { chat_id: Option<String>, #[arg(long)] command_id: Option<String> },
    Handoff {
        chat_id: Option<String>,
        #[arg(long)] prompt_file: PathBuf,
        #[arg(long, default_value="local", value_parser=["local","staging_snapshot","production_snapshot"])] database_environment: String,
        /// Persist this ID when recovering a lost acknowledgement. The default is the exact request digest.
        #[arg(long)] command_id: Option<String>,
        #[arg(long, requires="recover_sandbox_id")] recover_chat_id: Option<String>,
        #[arg(long, requires="recover_chat_id")] recover_sandbox_id: Option<String>,
    },
    Send {
        chat_id: String, text: String,
        #[arg(long)] from: Option<String>,
        #[arg(long)] command_id: Option<String>,
        #[arg(long)] wait: bool,
        #[arg(long, value_parser=clap::value_parser!(u64).range(0..=120000))] timeout: Option<u64>,
    },
    Reply {
        #[arg(long)] session: String,
        #[arg(long)] command: String,
        text: String,
        #[arg(long)] command_id: Option<String>,
        #[arg(long)] wait: bool,
    },
    Wait {
        #[arg(long)] session: String,
        #[arg(long)] thread: String,
        #[arg(long, value_parser=clap::value_parser!(u64).range(0..=120000))] timeout: Option<u64>,
    },
}

pub fn current(explicit: Option<String>) -> anyhow::Result<String> {
    explicit.or_else(|| std::env::var("COMET_SESSION_ID").ok())
        .map(|id| id.trim().to_owned()).filter(|id| !id.is_empty())
        .context("no source session: pass its ID or run inside a Crew agent session")
}
fn prompt_file(path: &Path) -> anyhow::Result<String> {
    let mut options = std::fs::OpenOptions::new(); options.read(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).context("cannot open prompt file")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() { bail!("prompt must be a regular file"); }
    let mut bytes = Vec::new(); file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 { bail!("prompt exceeds 1 MiB"); }
    let prompt = String::from_utf8(bytes).context("prompt must be UTF-8")?;
    if prompt.trim().is_empty() { bail!("prompt must not be empty"); }
    Ok(prompt)
}
fn print_json(value: &Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?); Ok(())
}
pub async fn connect(port: u16) -> anyhow::Result<zeron_rpc::RpcClient> {
    zeron_rpc::connect_ws(&format!("ws://127.0.0.1:{port}")).await
        .with_context(|| format!("no Crew engine listening on 127.0.0.1:{port}; start the matching isolated engine"))
}
pub async fn run(command: SessionCommand, port: u16) -> anyhow::Result<()> {
    if matches!(&command, SessionCommand::Current) { println!("{}", current(None)?); return Ok(()); }
    let client = connect(port).await?;
    let (method, params) = match command {
        SessionCommand::Current => unreachable!(),
        SessionCommand::Read { chat_id } => {
            let mut stream = client.subscribe("WatchDocMessages", json!({"chatId":chat_id})).await?;
            let snapshot = stream.recv().await.context("transcript stream ended before its snapshot")?;
            return print_json(&snapshot);
        }
        SessionCommand::Search { query, source_url, owner_id, limit, cursor } =>
            ("SearchSessions", json!({"query":query,"sourceUrl":source_url,"ownerId":owner_id,"limit":limit,"cursor":cursor})),
        SessionCommand::Fork { chat_id, command_id } =>
            ("ForkSession", json!({"sourceChatId":current(chat_id)?,"commandId":command_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string())})),
        SessionCommand::Handoff { chat_id, prompt_file: path, database_environment, command_id, recover_chat_id, recover_sandbox_id } => {
            let recovering = recover_chat_id.is_some();
            let mut params = json!({"sourceChatId":current(chat_id)?,"prompt":prompt_file(&path)?,
                "databaseEnvironment":database_environment,"recoverChatId":recover_chat_id,"recoverSandboxId":recover_sandbox_id});
            let command_id = command_id.unwrap_or_else(|| if recovering { String::new() }
                else { format!("native-{:x}", Sha256::digest(serde_json::to_vec(&params).expect("JSON input"))) });
            params["commandId"] = command_id.clone().into();
            let method = if recovering { "RecoverSessionHandoffToScaffold" } else { "HandoffSessionToScaffold" };
            let receipt = client.call(method, params).await.with_context(|| format!(
                "native handoff command {command_id:?} was not retried automatically; inspect the durable target in Crew, then recover with its exact IDs (no standalone handoff fallback)"))?;
            return print_json(&receipt);
        }
        SessionCommand::Send { chat_id, text, from, command_id, wait, timeout } =>
            ("SendPeerMessage", json!({"sourceChatId":current(from)?,"targetChatId":chat_id,"text":text,
                "commandId":command_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),"wait":wait,"timeoutMs":timeout})),
        SessionCommand::Reply { session, command, text, command_id, wait } =>
            ("ReplyPeerMessage", json!({"sessionId":session,"commandId":command,"text":text,
                "sendCommandId":command_id.unwrap_or_else(|| format!("reply:{command}")),"wait":wait})),
        SessionCommand::Wait { session, thread, timeout } =>
            ("WaitPeerReply", json!({"sourceChatId":session,"threadId":thread,"timeoutMs":timeout})),
    };
    let command_id = params["sendCommandId"].as_str().or_else(|| params["commandId"].as_str()).map(str::to_owned);
    print_json(&client.call(method, params).await.with_context(|| format!("{method} failed; retain command ID {command_id:?} for an explicit retry; no automatic retry was attempted"))?)
}

#[derive(Debug, Subcommand)]
pub enum ScaffoldCommand {
    /// List the current verified deployment's environments.
    List,
    /// Submit a typed ScaffoldEnvironmentControl JSON request (inspect/create/pause/resume/stop/attach/updateAgentRoute).
    Control { #[arg(long)] command_id: String, #[arg(long)] request_file: PathBuf },
    /// Read or reconcile an immutable operation; never replay an ambiguous create.
    Outcome { command_id: String, #[arg(long)] reconcile: bool },
}
pub async fn scaffold(command: ScaffoldCommand, port: u16) -> anyhow::Result<()> {
    let client = connect(port).await?;
    let value = match command {
        ScaffoldCommand::List => {
            let status = client.call("crew.status", json!({})).await?;
            client.call("scaffold.refresh", status.get("scope").cloned().context("verified scope unavailable")?).await?
        }
        ScaffoldCommand::Control { command_id, request_file } => {
            let control: zeron_proto::ScaffoldEnvironmentControl = serde_json::from_str(&prompt_file(&request_file)?)?;
            client.call("scaffold.control", json!({"commandId":command_id,"control":control})).await?
        }
        ScaffoldCommand::Outcome { command_id, reconcile } => client.call(
            if reconcile { "scaffold.reconcile" } else { "scaffold.outcome" }, json!({"commandId":command_id})).await?,
    };
    print_json(&value)
}
pub async fn import(backup: PathBuf, destination: PathBuf, port: u16) -> anyhow::Result<()> {
    let backup = backup.canonicalize()?;
    let destination = std::path::absolute(destination)?;
    print_json(&connect(port).await?.call("ImportCrewBackup", json!({"backup":backup,"destination":destination})).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompt_reader_bounds_input_without_consuming_it() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("prompt");
        std::fs::write(&path, "native task").unwrap();
        assert_eq!(prompt_file(&path).unwrap(), "native task");
        assert!(path.exists());
        std::fs::write(&path, vec![b'x'; 1024 * 1024 + 1]).unwrap();
        assert!(prompt_file(&path).is_err());
    }
}
