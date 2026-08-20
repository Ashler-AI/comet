use anyhow::{Context, anyhow};
use clap::Subcommand;

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

    let client = comet_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .map_err(|error| {
            anyhow!("no engine listening on 127.0.0.1:{ipc_port} ({error}) — is comet running?")
        })?;

    match command {
        SessionCommand::Current => unreachable!("handled before connecting"),
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
        .ok_or_else(|| anyhow!("no source session: pass --from or run inside a Crew agent session"))
}

fn print_json(value: &serde_json::Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_session_id;

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
}
