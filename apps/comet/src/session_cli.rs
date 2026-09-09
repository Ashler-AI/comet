use anyhow::{Context, anyhow};
use clap::Subcommand;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// Print the current session id supplied to this agent run.
    Current,
    /// Add an exact session id to this workspace's shared sessions.
    Add { chat_id: String },
    /// Remove a shared-session reference from this workspace.
    Remove { chat_id: String },
    /// Read the current transcript snapshot for a session.
    Read { chat_id: String },
    /// Fork a Crew session, preserving its context and harness/model configuration.
    Fork { chat_id: Option<String> },
    /// Transfer a session's native context to Scaffold and queue a remote task.
    Handoff {
        /// Source session; defaults to COMET_SESSION_ID inside an agent run.
        chat_id: Option<String>,
        /// UTF-8 task prompt file, nonempty and at most 1 MiB.
        #[arg(long, value_name = "FILE")]
        prompt_file: PathBuf,
        /// Database environment: local, staging_snapshot, or production_snapshot.
        #[arg(long, default_value = "local", value_parser = parse_database_environment)]
        database_environment: comet_proto::ScaffoldDatabaseEnvironment,
    },
    /// Send a message to another session.
    Send {
        chat_id: String,
        text: String,
        /// Source session; defaults to COMET_SESSION_ID inside an agent run.
        #[arg(long, value_name = "CHAT_ID")]
        from: Option<String>,
        /// Wait atomically for the target session's reply.
        #[arg(long)]
        wait: bool,
        /// Reply wait timeout in milliseconds (maximum 120000).
        #[arg(long, value_name = "MS")]
        timeout: Option<u64>,
    },
    /// Reply to a peer-message command received by this session.
    Reply {
        #[arg(long, value_name = "CHAT_ID")]
        session: String,
        #[arg(long, value_name = "COMMAND_ID")]
        command: String,
        text: String,
        /// Wait atomically for the next reply in the thread.
        #[arg(long)]
        wait: bool,
    },
    /// Wait for the next reply in an existing peer-message thread.
    Wait {
        #[arg(long, value_name = "CHAT_ID")]
        session: String,
        #[arg(long, value_name = "THREAD_ID")]
        thread: String,
        /// Wait timeout in milliseconds (maximum 120000).
        #[arg(long, value_name = "MS")]
        timeout: Option<u64>,
    },
}

pub async fn run(command: SessionCommand, ipc_port: u16) -> anyhow::Result<()> {
    if matches!(&command, SessionCommand::Current) {
        println!("{}", current_session_id(None)?);
        return Ok(());
    }

    let command = match command {
        SessionCommand::Handoff {
            chat_id,
            prompt_file,
            database_environment,
        } => {
            let params = comet_rpc::HandoffSessionToScaffoldParams {
                source_chat_id: current_session_id(chat_id)?,
                prompt: read_handoff_prompt(&prompt_file)?,
                database_environment,
            };
            let client = connect_engine(ipc_port).await?;
            let receipt: comet_rpc::HandoffSessionToScaffoldResult = client
                .call_as(
                    comet_rpc::methods::HANDOFF_SESSION_TO_SCAFFOLD,
                    serde_json::to_value(params)?,
                )
                .await
                .map_err(handoff_error)?;
            println!("{}", serde_json::to_string_pretty(&receipt)?);
            return Ok(());
        }
        command => command,
    };

    let client = connect_engine(ipc_port).await?;

    match command {
        SessionCommand::Current | SessionCommand::Handoff { .. } => {
            unreachable!("handled before connecting")
        }
        SessionCommand::Add { chat_id } => {
            let value = client
                .call(
                    comet_rpc::methods::ADD_SESSION_REF,
                    serde_json::to_value(comet_rpc::SessionRefParams { chat_id })?,
                )
                .await
                .context("AddSessionRef failed")?;
            print_json(&value)?;
        }
        SessionCommand::Remove { chat_id } => {
            let value = client
                .call(
                    comet_rpc::methods::REMOVE_SESSION_REF,
                    serde_json::to_value(comet_rpc::SessionRefParams { chat_id })?,
                )
                .await
                .context("RemoveSessionRef failed")?;
            print_json(&value)?;
        }
        SessionCommand::Fork { chat_id } => {
            let source_chat_id = current_session_id(chat_id)?;
            let value = client
                .call(
                    comet_rpc::methods::FORK_SESSION,
                    serde_json::to_value(comet_rpc::ForkSessionParams { source_chat_id })?,
                )
                .await
                .context("ForkSession failed")?;
            print_json(&value)?;
        }
        SessionCommand::Read { chat_id } => {
            let mut snapshots = client
                .subscribe(
                    comet_rpc::methods::WATCH_DOC_MESSAGES,
                    serde_json::json!({ "chatId": chat_id }),
                )
                .await
                .context("WatchDocMessages failed")?;
            let snapshot = snapshots
                .recv()
                .await
                .ok_or_else(|| anyhow!("WatchDocMessages ended before returning a transcript"))?;
            print_json(&snapshot)?;
        }
        SessionCommand::Send {
            chat_id,
            text,
            from,
            wait,
            timeout,
        } => {
            let source_chat_id = current_session_id(from)?;
            let params = comet_rpc::SendPeerMessageParams {
                source_chat_id,
                target_chat_id: chat_id,
                text,
                command_id: None,
                wait,
                timeout_ms: timeout,
            };
            let value = client
                .call(
                    comet_rpc::methods::SEND_PEER_MESSAGE,
                    serde_json::to_value(params)?,
                )
                .await
                .context("SendPeerMessage failed")?;
            print_json(&value)?;
        }
        SessionCommand::Reply {
            session,
            command,
            text,
            wait,
        } => {
            let params = comet_rpc::ReplyPeerMessageParams {
                session_id: session,
                command_id: command,
                text,
                wait,
                timeout_ms: None,
            };
            let value = client
                .call(
                    comet_rpc::methods::REPLY_PEER_MESSAGE,
                    serde_json::to_value(params)?,
                )
                .await
                .context("ReplyPeerMessage failed")?;
            print_json(&value)?;
        }
        SessionCommand::Wait {
            session,
            thread,
            timeout,
        } => {
            let params = comet_rpc::WaitPeerReplyParams {
                source_chat_id: session,
                thread_id: thread,
                timeout_ms: timeout,
            };
            let value = client
                .call(
                    comet_rpc::methods::WAIT_PEER_REPLY,
                    serde_json::to_value(params)?,
                )
                .await
                .context("WaitPeerReply failed")?;
            print_json(&value)?;
        }
    }
    Ok(())
}

async fn connect_engine(ipc_port: u16) -> anyhow::Result<comet_rpc::RpcClient> {
    comet_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .map_err(|error| {
            anyhow!("no engine listening on 127.0.0.1:{ipc_port} ({error}) — is comet running?")
        })
}

fn parse_database_environment(
    value: &str,
) -> Result<comet_proto::ScaffoldDatabaseEnvironment, String> {
    match value {
        "local" => Ok(comet_proto::ScaffoldDatabaseEnvironment::Local),
        "staging_snapshot" => Ok(comet_proto::ScaffoldDatabaseEnvironment::StagingSnapshot),
        "production_snapshot" => Ok(comet_proto::ScaffoldDatabaseEnvironment::ProductionSnapshot),
        _ => Err("expected local, staging_snapshot, or production_snapshot".into()),
    }
}

const MAX_HANDOFF_PROMPT_BYTES: usize = 1024 * 1024;

fn read_handoff_prompt(path: &Path) -> anyhow::Result<String> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("cannot read prompt file {}", path.display()))?;
    if !metadata.is_file() {
        return Err(anyhow!(
            "prompt file must be a regular file: {}",
            path.display()
        ));
    }
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot open prompt file {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((MAX_HANDOFF_PROMPT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read prompt file {}", path.display()))?;
    if bytes.len() > MAX_HANDOFF_PROMPT_BYTES {
        return Err(anyhow!(
            "prompt file exceeds the 1 MiB limit: {}",
            path.display()
        ));
    }
    let prompt = String::from_utf8(bytes)
        .with_context(|| format!("prompt file must contain UTF-8 text: {}", path.display()))?;
    if prompt.trim().is_empty() {
        return Err(anyhow!("prompt file must not be empty: {}", path.display()));
    }
    Ok(prompt)
}

fn handoff_error(error: comet_rpc::RpcError) -> anyhow::Error {
    // The wire protocol transmits server errors as strings, including UnknownMethod.
    let unknown_method = matches!(&error, comet_rpc::RpcError::UnknownMethod(_))
        || matches!(&error, comet_rpc::RpcError::Failed(message)
            if message.strip_prefix("unknown method: ") == Some(comet_rpc::methods::HANDOFF_SESSION_TO_SCAFFOLD));
    if unknown_method {
        anyhow!(
            "the running Crew engine does not support native Scaffold handoff; update the engine to match this comet binary ({error})"
        )
    } else {
        anyhow!(error).context(
            "native Scaffold handoff failed; it was not retried automatically. Check Crew for a created remote session before retrying",
        )
    }
}

fn current_session_id(explicit: Option<String>) -> anyhow::Result<String> {
    resolve_session_id(explicit, std::env::var("COMET_SESSION_ID").ok())
}

fn resolve_session_id(
    explicit: Option<String>,
    environment: Option<String>,
) -> anyhow::Result<String> {
    explicit
        .or(environment)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("no source session: pass a session id or run inside a Crew agent session")
        })
}

fn print_json(value: &serde_json::Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_HANDOFF_PROMPT_BYTES, read_handoff_prompt, resolve_session_id};

    #[test]
    fn explicit_source_wins_over_environment_context() {
        assert_eq!(
            resolve_session_id(
                Some(" explicit-session ".into()),
                Some("environment-session".into())
            )
            .unwrap(),
            "explicit-session"
        );
    }

    #[test]
    fn environment_context_supplies_default_source() {
        assert_eq!(
            resolve_session_id(None, Some(" environment-session ".into())).unwrap(),
            "environment-session"
        );
    }

    #[test]
    fn empty_explicit_source_is_rejected_instead_of_falling_back() {
        assert!(resolve_session_id(Some("  ".into()), Some("environment-session".into())).is_err());
    }

    #[test]
    fn handoff_prompt_preserves_utf8_and_whitespace_at_size_limit() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let prompt = format!("\n{}é\n", "x".repeat(MAX_HANDOFF_PROMPT_BYTES - 4));
        std::fs::write(file.path(), &prompt).unwrap();
        assert_eq!(read_handoff_prompt(file.path()).unwrap(), prompt);

        std::fs::write(file.path(), "x".repeat(MAX_HANDOFF_PROMPT_BYTES + 1)).unwrap();
        assert!(read_handoff_prompt(file.path()).is_err());
    }

    #[test]
    fn handoff_prompt_rejects_empty_whitespace_and_invalid_utf8() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(read_handoff_prompt(file.path()).is_err());
        std::fs::write(file.path(), " \n\t ").unwrap();
        assert!(read_handoff_prompt(file.path()).is_err());
        std::fs::write(file.path(), [0xff]).unwrap();
        assert!(read_handoff_prompt(file.path()).is_err());
    }

    #[test]
    fn handoff_prompt_rejects_missing_and_non_file_paths() {
        let directory = tempfile::tempdir().unwrap();
        assert!(read_handoff_prompt(directory.path()).is_err());
        assert!(read_handoff_prompt(&directory.path().join("missing")).is_err());
    }
}
