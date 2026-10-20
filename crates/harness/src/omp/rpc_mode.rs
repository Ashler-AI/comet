//! OMP harness transport over OMP's native RPC mode (`omp --mode rpc`).
//!
//! Newline-delimited JSON frames over stdio (see OMP's rpc.md). Unlike the ACP
//! lane — whose `session/prompt` is strictly turn-serial — RPC mode accepts
//! `prompt { streamingBehavior: "steer" }` at any time: idle it starts the next
//! turn, streaming it steers the live turn between tool calls (OMP's default
//! `interruptMode: "immediate"`). That is what lets Comet drive OMP as a
//! step-boundary harness.
//!
//! - Command responses are matched by string id (pending map resolved directly
//!   by the reader task). Prompt acks are matched inline by the session loop.
//! - Protocol v2 is negotiated when advertised: oversized stdout objects arrive
//!   as `rpc_chunk` sequences and are reassembled losslessly.
//! - Turn errors surface on the assistant `message_end` (`stopReason: "error"`
//!   + `errorMessage`), observed live against omp 17.2.9.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use std::sync::Mutex;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use crate::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tokio::time::Instant;

use zeron_proto::{AgentEvent, DoneStatus, SlashCommand, HarnessId, TodoItem, ToolCall, UserInputQuestion};
use crate::{HarnessError, RunControls};
const OMP_GOAL_STATE_CALL_ID: &str = "omp-goal-state";
const OMP_GOAL_STATE_CALL_NAME: &str = "OMP goal";
const STATE_DEADLINE: Duration = Duration::from_secs(15);
const INACTIVITY_LOG_INTERVAL: Duration = Duration::from_secs(300);
/// Command metadata can change while skills, extensions, and MCP prompts finish
/// startup. Keep the newest snapshot until the stream is briefly quiet instead
/// of killing the catalog process after its first partial update.
const COMMAND_CATALOG_SETTLE: Duration = Duration::from_secs(2);

/// A frame from the RPC child that the session loop consumes: everything
/// except responses to requests awaited through the pending map.
#[derive(Debug)]
pub(crate) enum RpcFrame {
    Frame(Value),
    Eof,
}

type Pending = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<Result<Value, String>>>>>;

#[derive(Clone)]
pub(crate) struct RpcModeClient {
    writer: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

impl RpcModeClient {
    pub(crate) fn new(stdin: ChildStdin, stdout: ChildStdout) -> (Self, mpsc::Receiver<RpcFrame>) {
        let (writer, writer_rx) = mpsc::unbounded_channel::<String>();
        let (frames_tx, frames_rx) = mpsc::channel::<RpcFrame>(256);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(write_loop(stdin, writer_rx));
        tokio::spawn(read_loop(stdout, pending.clone(), frames_tx));
        (
            Self {
                writer,
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
            },
            frames_rx,
        )
    }

    fn mint_id(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Send a command without awaiting its response. The response (matched by
    /// the returned id) is forwarded to the session loop as an ordinary frame.
    pub(crate) fn send_command(&self, kind: &str, mut fields: Map<String, Value>) -> String {
        let id = self.mint_id("f");
        fields.insert("id".into(), Value::String(id.clone()));
        fields.insert("type".into(), Value::String(kind.into()));
        let _ = self.writer.send(Value::Object(fields).to_string());
        id
    }

    /// Send a raw frame that is not a command (UI responses, host results).
    pub(crate) fn send_frame(&self, frame: Value) {
        let _ = self.writer.send(frame.to_string());
    }

    /// Send a command and await its response; returns the `data` payload.
    pub(crate) async fn request(
        &self,
        kind: &str,
        mut fields: Map<String, Value>,
    ) -> Result<Value, HarnessError> {
        let id = self.mint_id("c");
        fields.insert("id".into(), Value::String(id.clone()));
        fields.insert("type".into(), Value::String(kind.into()));
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id.clone(), tx);
        if self.writer.send(Value::Object(fields).to_string()).is_err() {
            self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
            return Err(HarnessError::Protocol(
                "OMP RPC child stdin closed".to_string(),
            ));
        }
        let result = tokio::time::timeout(STATE_DEADLINE, rx).await;
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
        match result {
            Ok(Ok(Ok(data))) => Ok(data),
            Ok(Ok(Err(message))) => Err(HarnessError::Protocol(format!("OMP RPC {kind}: {message}"))),
            Ok(Err(_)) => Err(HarnessError::Protocol(format!("OMP RPC {kind}: child exited before responding"))),
            Err(_) => Err(HarnessError::Protocol(format!("OMP RPC {kind}: response timed out"))),
        }
    }
}

async fn write_loop(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = rx.recv().await {
        let mut framed = line.into_bytes();
        framed.push(b'\n');
        if let Err(error) = stdin.write_all(&framed).await {
            tracing::debug!(target: "zeron_harness::omp", %error, "RPC-mode stdin write failed");
        }
    }
}

/// In-flight `rpc_chunk` reassembly (protocol v2 lossless framing).
#[derive(Default)]
struct ChunkAssembly {
    chunk_id: String,
    count: u64,
    byte_length: u64,
    next_index: u64,
    bytes: Vec<u8>,
}

/// Fold one frame into the assembly state. Returns a completed logical frame
/// when the sequence finishes; a malformed sequence is dropped with a warning
/// (the doc requires rejecting interleaved or interrupted sequences).
fn assemble_chunk(active: &mut Option<ChunkAssembly>, frame: &Value) -> Option<Value> {
    let chunk_id = frame.get("chunkId").and_then(Value::as_str)?;
    let index = frame.get("index").and_then(Value::as_u64)?;
    let count = frame.get("count").and_then(Value::as_u64)?;
    let byte_length = frame.get("byteLength").and_then(Value::as_u64)?;
    let data = frame.get("data").and_then(Value::as_str)?;

    if index == 0 {
        *active = Some(ChunkAssembly {
            chunk_id: chunk_id.to_string(),
            count,
            byte_length,
            next_index: 0,
            bytes: Vec::new(),
        });
    }
    let assembly = active.as_mut()?;
    if assembly.chunk_id != chunk_id
        || assembly.count != count
        || assembly.byte_length != byte_length
        || assembly.next_index != index
    {
        tracing::warn!(target: "zeron_harness::omp", chunk_id, "RPC chunk sequence violated; dropping");
        *active = None;
        return None;
    }
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(data) else {
        tracing::warn!(target: "zeron_harness::omp", chunk_id, "RPC chunk was not valid base64; dropping");
        *active = None;
        return None;
    };
    assembly.bytes.extend_from_slice(&decoded);
    assembly.next_index += 1;
    if assembly.next_index < assembly.count {
        return None;
    }
    let assembly = active.take()?;
    if assembly.bytes.len() as u64 != assembly.byte_length {
        tracing::warn!(
            target: "zeron_harness::omp",
            chunk_id = %assembly.chunk_id,
            expected = assembly.byte_length,
            actual = assembly.bytes.len(),
            "RPC chunk reassembly length mismatch; dropping"
        );
        return None;
    }
    match serde_json::from_slice::<Value>(&assembly.bytes) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(target: "zeron_harness::omp", %error, "RPC chunk reassembly was not JSON; dropping");
            None
        }
    }
}

/// True when this response resolves an awaited request; otherwise the frame
/// belongs to the session loop (prompt acks and unsolicited responses).
fn resolve_pending(pending: &Pending, frame: &Value) -> bool {
    let Some(id) = frame.get("id").and_then(Value::as_str) else {
        return false;
    };
    let Some(sender) = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id) else {
        return false;
    };
    let result = if frame.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(frame.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(frame
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("command failed")
            .to_string())
    };
    let _ = sender.send(result);
    true
}

async fn read_loop(stdout: ChildStdout, pending: Pending, tx: mpsc::Sender<RpcFrame>) {
    let mut lines = BufReader::new(stdout).lines();
    let mut active_chunks: Option<ChunkAssembly> = None;
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            tracing::debug!(target: "zeron_harness::omp", "skipping non-JSON RPC-mode line");
            continue;
        };
        let frame = match frame.get("type").and_then(Value::as_str) {
            Some("rpc_chunk") => match assemble_chunk(&mut active_chunks, &frame) {
                Some(reassembled) => reassembled,
                None => continue,
            },
            _ => frame,
        };
        if frame.get("type").and_then(Value::as_str) == Some("response")
            && resolve_pending(&pending, &frame)
        {
            continue;
        }
        if tx.send(RpcFrame::Frame(frame)).await.is_err() {
            return;
        }
    }
    // EOF: fail every awaited request, then tell the session loop.
    pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
    let _ = tx.send(RpcFrame::Eof).await;
}

pub(crate) struct ProcessGroupGuard {
    group: Option<i32>,
    #[cfg(unix)]
    birth: Option<String>,
    supervised: bool,
}

impl ProcessGroupGuard {
    pub(crate) fn new(group: Option<i32>, supervised: bool) -> Self {
        Self {
            #[cfg(unix)]
            birth: group.and_then(|pid| super::process_birth(pid as u32)),
            group,
            supervised,
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.filter(|group| {
            self.birth.is_some()
                && super::process_birth(*group as u32) == self.birth
                && *group > 1
                && {
                    // SAFETY: getpgrp has no preconditions.
                    *group != unsafe { libc::getpgrp() }
                }
        }) {
            // Leave the verified supervisor alive to reap OMP and detached tools.
            // Unsupervised children still need the dedicated group's hard stop.
            unsafe {
                libc::kill(
                    -group,
                    if self.supervised {
                        libc::SIGTERM
                    } else {
                        libc::SIGKILL
                    },
                );
            }
        }
    }
}

pub(crate) struct RpcModeProcess {
    pub child: Child,
    /// Dedicated process group for this persistent run (supervisor + OMP +
    /// tool descendants). `None` only for one-shot catalogs and legacy tests.
    pub process_group: Option<i32>,
    /// Drop guard for task cancellation, panic unwind, and setup failures.
    pub process_group_guard: ProcessGroupGuard,
    pub client: RpcModeClient,
    pub frames: mpsc::Receiver<RpcFrame>,
    pub stderr_tail: crate::StderrTail,
    pub run_config: Option<super::OmpRunConfig>,
}

async fn kill_rpc_process(
    child: &mut Child,
    process_group: Option<i32>,
    interrupt_grace: Duration,
) {
    #[cfg(not(unix))]
    let _ = (process_group, interrupt_grace);
    #[cfg(unix)]
    if process_group.is_some() && child.id() != process_group.map(|group| group as u32) {
        return;
    }
    #[cfg(unix)]
    let birth = process_group.and_then(|pid| super::process_birth(pid as u32));
    #[cfg(unix)]
    if let Some(group) = process_group.filter(|group| {
        birth.is_some() && super::process_birth(*group as u32) == birth && *group > 1 && {
            // SAFETY: getpgrp has no preconditions.
            *group != unsafe { libc::getpgrp() }
        }
    }) {
        // The supervisor needs its two-second grace even if the RPC grace is shorter.
        unsafe {
            libc::kill(-group, libc::SIGTERM);
        }
        let deadline = Instant::now() + interrupt_grace.max(Duration::from_secs(3));
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                _ => break,
            }
        }
        if super::process_birth(group as u32) == birth {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        let _ = child.wait().await;
        return;
    }
    let _ = child.kill().await;
}

fn rpc_assistant_message_id(session_id: &str, nonce: &uuid::Uuid, turn: u64) -> String {
    format!("omp-rpc-{session_id}-{nonce}-{turn}")
}

fn image_content(attachments: &[String]) -> Vec<Value> {
    attachments
        .iter()
        .filter_map(|attachment| {
            let bytes = std::fs::read(attachment).ok()?;
            let mime_type = match std::path::Path::new(attachment)
                .extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("png") => "image/png",
                Some("gif") => "image/gif",
                Some("webp") => "image/webp",
                _ => "image/jpeg",
            };
            Some(json!({
                "type": "image",
                "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                "mimeType": mime_type,
            }))
        })
        .collect()
}

fn prompt_fields(message: &str, images: Vec<Value>) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("message".into(), Value::String(message.to_string()));
    if !images.is_empty() {
        fields.insert("images".into(), Value::Array(images));
    }
    // Legal in every state: idle it starts the next turn, streaming it steers
    // the live turn. This closes the idle/streaming race entirely.
    fields.insert("streamingBehavior".into(), Value::String("steer".into()));
    fields
}

/// Map a tool invocation to Comet's rendered call kinds. RPC mode hands us the
/// real tool name and arguments, so common tools render natively instead of
/// collapsing into an opaque generic block.
fn tool_call_from_start(frame: &Value) -> Option<AgentEvent> {
    let id = frame.get("toolCallId")?.as_str()?.to_string();
    let name = frame.get("toolName").and_then(Value::as_str).unwrap_or("");
    let args = frame.get("args").cloned().unwrap_or(Value::Null);
    let arg_str = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|value| !value.trim().is_empty())
    };
    let call = match name {
        "bash" => arg_str("command").map(|command| ToolCall::Exec { command }),
        "read" => arg_str("path").map(|path| ToolCall::ReadFile { path }),
        "write" => arg_str("path").map(|path| ToolCall::WriteFile {
            path,
            content: arg_str("content"),
        }),
        "edit" => arg_str("path").map(|path| ToolCall::EditFile {
            path,
            old_string: arg_str("oldText").or_else(|| arg_str("old_string")),
            new_string: arg_str("newText").or_else(|| arg_str("new_string")),
        }),
        "grep" => arg_str("pattern").map(|pattern| ToolCall::Search {
            pattern,
            path: arg_str("path"),
        }),
        "glob" => arg_str("pattern").map(|pattern| ToolCall::Glob { pattern }),
        "web_search" => arg_str("query").map(|query| ToolCall::WebSearch { query }),
        "task" => Some(ToolCall::Unknown { name: "Agent".into(), input: Some(args.clone()) }),
        _ => None,
    }
    .unwrap_or_else(|| ToolCall::Unknown {
        name: frame
            .get("intent")
            .and_then(Value::as_str)
            .filter(|intent| !intent.trim().is_empty())
            .unwrap_or(if name.is_empty() { "OMP tool" } else { name })
            .to_string(),
        input: (!args.is_null()).then(|| args.clone()),
    });
    Some(AgentEvent::ToolCall { id, call })
}

fn result_text(result: &Value) -> Option<String> {
    if let Some(text) = result.as_str() {
        return (!text.is_empty()).then(|| text.to_string());
    }
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        let joined = blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    (!result.is_null())
        .then(|| serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string()))
}

fn assistant_message_event(frame: &Value) -> Option<AgentEvent> {
    let event = frame.get("assistantMessageEvent")?;
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
    match kind {
        "text_delta" if !delta.is_empty() => Some(AgentEvent::TextDelta { text: delta.into() }),
        "thinking_delta" if !delta.is_empty() => {
            Some(AgentEvent::ReasoningDelta { text: delta.into() })
        }
        _ => None,
    }
}

fn command_output_event(frame: &Value) -> Option<AgentEvent> {
    frame
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| AgentEvent::TextDelta {
            text: format!("    {}", text.replace('\n', "\n    ")),
        })
}

fn tool_result_from_end(frame: &Value) -> Option<AgentEvent> {
    let id = frame
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or("omp-tool")
        .to_string();
    Some(AgentEvent::ToolResult {
        id,
        is_error: frame.get("isError").and_then(Value::as_bool) == Some(true),
        diff: None,
        output: frame
            .get("result")
            .and_then(result_text)
            .map(|mut output| {
                let mut end = output.len().min(16_384);
                while !output.is_char_boundary(end) { end -= 1; }
                output.truncate(end);
                output
            }),
    })
}
/// The error carried by an assistant message that ended with
/// `stopReason: "error"` (observed live: `errorMessage` is the provider's
/// error body).
fn assistant_error(message: &Value) -> Option<String> {
    if message.get("stopReason").and_then(Value::as_str) != Some("error") {
        return None;
    }
    Some(
        message
            .get("errorMessage")
            .and_then(Value::as_str)
            .unwrap_or("OMP reported a model error")
            .to_string(),
    )
}

fn usage_event(message: &Value) -> Option<AgentEvent> {
    let usage = message.get("usage")?;
    let read = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let input_tokens = read("input") + read("cacheRead") + read("cacheWrite");
    let output_tokens = read("output");
    (input_tokens > 0 || output_tokens > 0).then_some(AgentEvent::Usage {
        input_tokens,
        output_tokens,
    })
}

fn todo_items_from_state(state: &Value) -> Vec<TodoItem> {
    state
        .get("todoPhases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|phase| phase.get("tasks").and_then(Value::as_array))
        .flatten()
        .filter_map(|task| {
            let text = task
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())?;
            Some(TodoItem {
                text: text.to_string(),
                done: task.get("status").and_then(Value::as_str) == Some("completed"),
                status: task.get("status").cloned().and_then(|value| serde_json::from_value(value).ok()),
            })
        })
        .collect()
}

fn goal_state_event(goal: Value, state: Option<Value>) -> AgentEvent {
    AgentEvent::ToolCall {
        id: OMP_GOAL_STATE_CALL_ID.into(),
        call: ToolCall::Unknown {
            name: OMP_GOAL_STATE_CALL_NAME.into(),
            input: Some(json!({ "goal": goal, "state": state })),
        },
    }
}

fn goal_state_event_from_session_state(state: &Value) -> Option<AgentEvent> {
    let goal_state = state
        .get("goalMode")
        .or_else(|| state.get("goal_mode"))?
        .clone();
    let goal = goal_state.get("goal").cloned().unwrap_or(Value::Null);
    Some(goal_state_event(goal, Some(goal_state)))
}

fn goal_state_event_from_frame(frame: &Value) -> Option<AgentEvent> {
    (frame.get("type").and_then(Value::as_str) == Some("goal_updated")).then(|| {
        goal_state_event(
            frame.get("goal").cloned().unwrap_or(Value::Null),
            frame.get("state").cloned(),
        )
    })
}

fn commands_from_frame(frame: &Value) -> Option<Vec<SlashCommand>> {
    let mut commands: Vec<_> = frame.get("commands")?.as_array()?.iter().filter_map(|command| {
        Some(SlashCommand {
            name: command.get("name")?.as_str()?.to_owned(),
            description: command.get("description").and_then(Value::as_str).unwrap_or_default().to_owned(),
            input_hint: command.pointer("/input/hint").and_then(Value::as_str).map(str::to_owned),
        })
    }).collect();
    if !commands.iter().any(|command| command.name == "goal") {
        commands.push(SlashCommand { name: "goal".into(), description: "Manage this session's goal".into(), input_hint: Some("goal | status | drop".into()) });
    }
    Some(commands)
}

/// Bridge an RPC extension UI dialog onto Comet's question surface.
///
/// `confirm` and `select` map onto option questions; free-text `input` and
/// `editor` dialogs have no Comet surface and fail closed as cancelled (the
/// same posture as an unanswerable provider gate). Cosmetic methods
/// (`notify`, `setStatus`, `setWidget`, `setTitle`, `set_editor_text`) are
/// fire-and-forget and ignored.
fn handle_ui_request(frame: &Value, client: &RpcModeClient, request_input: &super::RequestInputFn) {
    let Some(id) = frame.get("id").and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
    let title = frame
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("OMP");
    let message = frame
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match method {
        "confirm" => {
            let receiver = request_input(vec![UserInputQuestion {
                id: "confirm".into(),
                header: title.to_string(),
                question: if message.is_empty() {
                    "Continue?".to_string()
                } else {
                    message.to_string()
                },
                options: vec!["Continue".into(), "Cancel".into()],
                multi_select: false,
                prefill: None,
                multiline: false,
            }]);
            let client = client.clone();
            tokio::spawn(async move {
                let confirmed = receiver.await.ok().is_some_and(|answers| {
                    answers
                        .iter()
                        .flat_map(|answer| &answer.labels)
                        .any(|label| label == "Continue")
                });
                client.send_frame(json!({
                    "type": "extension_ui_response",
                    "id": id,
                    "confirmed": confirmed,
                }));
            });
        }
        "select" => {
            let options: Vec<String> = frame
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    option.as_str().map(str::to_string).or_else(|| {
                        option
                            .get("label")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                })
                .collect();
            let receiver = request_input(vec![UserInputQuestion {
                id: "select".into(),
                header: title.to_string(),
                question: message.to_string(),
                options,
                multi_select: false,
                prefill: None,
                multiline: false,
            }]);
            let client = client.clone();
            tokio::spawn(async move {
                let selected = receiver.await.ok().and_then(|answers| {
                    answers.into_iter().flat_map(|answer| answer.labels).next()
                });
                match selected {
                    Some(value) => client.send_frame(json!({
                        "type": "extension_ui_response",
                        "id": id,
                        "value": value,
                    })),
                    None => client.send_frame(json!({
                        "type": "extension_ui_response",
                        "id": id,
                        "cancelled": true,
                    })),
                }
            });
        }
        "input" | "editor" => {
            client.send_frame(json!({
                "type": "extension_ui_response",
                "id": id,
                "cancelled": true,
            }));
        }
        _ => {}
    }
}

pub(crate) struct RpcRunOptions {
    pub process_label: &'static str,
    /// Canonical header identity of the journal selected by `--resume`; native RPC
    /// must resolve to the canonical journal header ID or the run fails closed.
    pub expected_resume: Option<String>,
}
#[derive(Debug)]
struct PendingPrompt {
    id: String,
    refresh_goal_state: bool,
}

impl PendingPrompt {
    fn new(id: String, prompt: &str) -> Self {
        Self {
            id,
            refresh_goal_state: is_goal_command(prompt),
        }
    }
}

fn is_goal_command(prompt: &str) -> bool {
    let Some(rest) = prompt.trim_start().strip_prefix("/goal") else {
        return false;
    };
    rest.is_empty() || rest.chars().next().is_some_and(char::is_whitespace)
}

/// Drive one persistent RPC-mode session: first prompt, streamed turns,
/// mid-turn steering, explicit aborts, and turn-error surfacing. Returns when
/// the steering mailbox closes, the run is interrupted, or the child dies.
pub(crate) async fn run_rpc(
    process: RpcModeProcess,
    request: zeron_proto::RunRequest,
    controls: RunControls,
    events: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    interrupt_grace: Duration,
    options: RpcRunOptions,
) -> Result<(), HarnessError> {
    let RpcModeProcess {
        mut child,
        client,
        mut frames,
        stderr_tail,
        process_group,
        process_group_guard: _process_group_guard,
        run_config: _run_config,
    } = process;
    let RunControls {
        request_input,
        mut steering,
        interrupt,
        context: _,
        execution_lease: _execution_lease,
        realtime: _,
        turn: turn_control,
    } = controls;
    let cleanup_interrupt = interrupt.clone();
    let cleanup_events = events.clone();
    let mut cancelled_during_startup = false;
    let result = tokio::select! {
        result = async {
    let crash = |child: &mut Child, label: &str| {
        HarnessError::Protocol(crate::crash_message(
            label,
            child.try_wait().ok().flatten(),
            &stderr_tail,
        ))
    };

    // Stage 1: the ready frame (always the first stdout object).
    let ready = tokio::time::timeout(super::RPC_STARTUP_DEADLINE, async {
        loop {
            match frames.recv().await {
                Some(RpcFrame::Frame(frame))
                    if frame.get("type").and_then(Value::as_str) == Some("ready") =>
                {
                    return Ok(frame);
                }
                Some(RpcFrame::Frame(_)) => continue,
                Some(RpcFrame::Eof) | None => Err::<Value, ()>(()),
            }?;
        }
    })
    .await
    .map_err(|_| {
        HarnessError::Protocol(format!(
            "{} ready frame timed out after {}s",
            options.process_label,
            super::RPC_STARTUP_DEADLINE.as_secs()
        ))
    })?
    .map_err(|()| crash(&mut child, options.process_label))?;

    // Stage 2: negotiate lossless framing when the server advertises v2.
    let supports_v2 = ready
        .get("supportedProtocolVersions")
        .and_then(Value::as_array)
        .is_some_and(|versions| versions.iter().any(|v| v.as_u64() == Some(2)));
    if supports_v2 {
        let mut fields = Map::new();
        fields.insert("protocolVersion".into(), json!(2));
        if let Err(error) = client.request("negotiate_protocol", fields).await {
            tracing::warn!(target: "zeron_harness::omp", %error, "protocol v2 negotiation failed; staying on v1");
        }
    }

    // Stage 3: session identity (and resume validation) from get_state.
    let state = tokio::time::timeout(STATE_DEADLINE, client.request("get_state", Map::new()))
        .await
        .map_err(|_| {
            HarnessError::Protocol(format!(
                "{} get_state timed out after {}s",
                options.process_label,
                STATE_DEADLINE.as_secs()
            ))
        })??;
    let session_id = state
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HarnessError::Protocol(format!(
                "{} get_state reported no sessionId",
                options.process_label
            ))
        })?
        .to_string();
    if let Some(expected) = options.expected_resume.as_deref()
        && session_id != expected
    {
        return Err(HarnessError::Protocol(format!(
            "{} resumed session {session_id} instead of {expected}",
            options.process_label
        )));
    }

    let model = state
        .get("model")
        .map(|model| {
            let provider = model.get("provider").and_then(Value::as_str).unwrap_or("");
            let id = model.get("id").and_then(Value::as_str).unwrap_or("");
            if provider.is_empty() {
                id.to_string()
            } else {
                format!("{provider}/{id}")
            }
        })
        .filter(|selector| !selector.is_empty())
        .or_else(|| request.model.clone())
        .unwrap_or_else(|| "default".into());

    let nonce = uuid::Uuid::new_v4();
    let mut turn = 0_u64;
    let mut assistant_message_id = rpc_assistant_message_id(&session_id, &nonce, turn);
    if events
        .send(Ok(AgentEvent::SessionStarted {
            harness: HarnessId::Omp,
            model,
            tools: vec![],
            cwd: request.cwd.clone(),
            session_id: session_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        }))
        .await
        .is_err()
    {
        kill_rpc_process(&mut child, process_group, interrupt_grace).await;
        return Ok(());
    }
    if let Some(goal_event) = goal_state_event_from_session_state(&state)
        && events.send(Ok(goal_event)).await.is_err()
    {
        kill_rpc_process(&mut child, process_group, interrupt_grace).await;
        return Ok(());
    }

    // Stage 4: first prompt. The ack is immediate (acceptance, not
    // completion); its response is matched inline by the session loop below.

    let mut outstanding_prompts = vec![PendingPrompt::new(
        client.send_command(
            "prompt",
            prompt_fields(&request.prompt, image_content(&request.attachments)),
        ),
        &request.prompt,
    )];

    let mut steering_open = true;
    let mut streaming = false;
    let mut turn_error: Option<String> = None;
    let mut done_emitted = false;
    let mut stopped = false;
    let inactivity = tokio::time::sleep(INACTIVITY_LOG_INTERVAL);
    tokio::pin!(inactivity);

    loop {
        tokio::select! {
            biased;
            frame = frames.recv() => {
                inactivity.as_mut().reset(Instant::now() + INACTIVITY_LOG_INTERVAL);
                let frame = match frame {
                    Some(RpcFrame::Frame(frame)) => frame,
                    Some(RpcFrame::Eof) | None => {
                        // The child never exits on its own while stdin is
                        // open: any EOF here is a crash, parked or not.
                        return Err(crash(&mut child, options.process_label));
                    }
                };
                match frame.get("type").and_then(Value::as_str).unwrap_or("") {
                    "agent_start" => {
                        streaming = true;
                        done_emitted = false;
                    }
                    "message_start" => {
                        if frame.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                            turn_error = None;
                        }
                    }
                    "message_update" => {
                        if let Some(event) = assistant_message_event(&frame)
                            && events.send(Ok(event)).await.is_err()
                        {
                            kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                            return Ok(());
                        }
                    }
                    "command_output" => {
                        if let Some(event) = command_output_event(&frame)
                            && events.send(Ok(event)).await.is_err()
                        {
                            kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                            return Ok(());
                        }
                    }
                    "message_end" => {
                        if let Some(message) = frame.get("message")
                            && message.get("role").and_then(Value::as_str) == Some("assistant")
                        {
                            turn_error = assistant_error(message);
                            if let Some(usage) = usage_event(message)
                                && events.send(Ok(usage)).await.is_err()
                            {
                                kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                                return Ok(());
                            }
                        }
                    }
                    "tool_execution_start" => {
                        if let Some(event) = tool_call_from_start(&frame)
                            && events.send(Ok(event)).await.is_err()
                        {
                            kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                            return Ok(());
                        }
                    }
                    "tool_execution_end" => {
                        let todo_tool = frame.get("toolName").and_then(Value::as_str) == Some("todo")
                            && frame.get("isError").and_then(Value::as_bool) != Some(true);
                        if let Some(event) = tool_result_from_end(&frame)
                            && events.send(Ok(event)).await.is_err()
                        {
                            kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                            return Ok(());
                        }
                        // The todo tool mutates session-held state; the frame
                        // carries only the op. Snapshot the resulting phases so
                        // Comet renders the plan like the ACP lane's `plan`
                        // updates did.
                        if todo_tool && let Ok(state) = client.request("get_state", Map::new()).await {
                            let items = todo_items_from_state(&state);
                            if events
                                .send(Ok(AgentEvent::ToolCall {
                                    id: zeron_proto::LIVE_PLAN_TOOL_ID.into(),
                                    call: ToolCall::Todo { items },
                                }))
                                .await
                                .is_err()
                            {
                                kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                                return Ok(());
                            }
                        }
                    }
                    "goal_updated" => {
                        if let Some(event) = goal_state_event_from_frame(&frame)
                            && events.send(Ok(event)).await.is_err()
                        {
                            kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                            return Ok(());
                        }
                    }
                    "agent_end" => {
                        if frame.get("isTerminal").and_then(Value::as_bool) == Some(false) {
                            continue;
                        }
                        streaming = false;
                        let status = if std::mem::take(&mut stopped) {
                            DoneStatus::Interrupted
                        } else if turn_error.is_some() {
                            DoneStatus::Errored
                        } else {
                            DoneStatus::Completed
                        };
                        // A terminal error closes the persistent RPC run. Reap its
                        // session writer before Done makes queued recovery dispatchable.
                        // Done carries the sole visible error; emitting Error as
                        // well would fold the same failure into two transcript cards.
                        if status == DoneStatus::Errored {
                            kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                        }
                        done_emitted = true;
                        if events
                            .send(Ok(AgentEvent::Done {
                                status,
                                result: None,
                                error: turn_error.take(),
                                session_id: Some(session_id.clone()),
                            }))
                            .await
                            .is_err()
                        {
                            kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                            return Ok(());
                        }
                        if status == DoneStatus::Errored {
                            return Ok(());
                        }
                    }
                    "prompt_result" => {
                        // A prompt accepted earlier resolved without invoking
                        // the agent (local slash command): close the turn.
                        if !streaming && !done_emitted {
                            done_emitted = true;
                            if events
                                .send(Ok(AgentEvent::Done {
                                    status: DoneStatus::Completed,
                                    result: None,
                                    error: None,
                                    session_id: Some(session_id.clone()),
                                }))
                                .await
                                .is_err()
                            {
                                kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                                return Ok(());
                            }
                        }
                    }
                    "response" => {
                        let id = frame.get("id").and_then(Value::as_str).unwrap_or("");
                        if let Some(index) = outstanding_prompts.iter().position(|prompt| prompt.id == id) {
                            let prompt = outstanding_prompts.swap_remove(index);
                            if frame.get("success").and_then(Value::as_bool) != Some(true) {
                                let message = frame
                                    .get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("prompt was rejected")
                                    .to_string();
                                kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                                return Err(HarnessError::Protocol(format!(
                                    "{} prompt failed: {message}",
                                    options.process_label
                                )));
                            }
                            // Goal commands mutate session state synchronously but OMP
                            // 17.2.9 does not reliably emit `goal_updated` over RPC.
                            // Snapshot after the ack so the editor indicator appears
                            // immediately and `/goal drop` clears persisted state.
                            if prompt.refresh_goal_state {
                                match client.request("get_state", Map::new()).await {
                                    Ok(state) => {
                                        let event = goal_state_event_from_session_state(&state)
                                            .unwrap_or_else(|| goal_state_event(Value::Null, None));
                                        if events.send(Ok(event)).await.is_err() {
                                            kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                                            return Ok(());
                                        }
                                    }
                                    Err(error) => tracing::warn!(
                                        target: "zeron_harness::omp",
                                        %error,
                                        "goal command state refresh failed"
                                    ),
                                }
                            }
                            // Local-only prompt: completed without agent events.
                            if frame.pointer("/data/agentInvoked").and_then(Value::as_bool)
                                == Some(false)
                                && !streaming
                                && !done_emitted
                            {
                                done_emitted = true;
                                if events
                                    .send(Ok(AgentEvent::Done {
                                        status: DoneStatus::Completed,
                                        result: None,
                                        error: None,
                                        session_id: Some(session_id.clone()),
                                    }))
                                    .await
                                    .is_err()
                                {
                                    kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                                    return Ok(());
                                }
                            }
                        }
                    }
                    "extension_ui_request" => {
                        handle_ui_request(&frame, &client, &request_input);
                    }
                    "host_tool_call" => {
                        if let Some(id) = frame.get("id").and_then(Value::as_str) {
                            client.send_frame(json!({
                                "type": "host_tool_result",
                                "id": id,
                                "isError": true,
                                "result": { "content": [{ "type": "text", "text": "Crew registers no host tools" }] },
                            }));
                        }
                    }
                    "host_uri_request" => {
                        if let Some(id) = frame.get("id").and_then(Value::as_str) {
                            client.send_frame(json!({
                                "type": "host_uri_result",
                                "id": id,
                                "isError": true,
                                "error": "Crew registers no host URI schemes",
                            }));
                        }
                    }
                    _ => {}
                }
            }
            steer = steering.recv(), if steering_open => match steer {
                Some(message) => {
                    // Step-boundary contract: the prompt reaches OMP now —
                    // steering a live turn, or starting the next one when
                    // parked. Rotate the assistant id either way so the new
                    // segment folds into a fresh message.
                    turn += 1;
                    let next = rpc_assistant_message_id(&session_id, &nonce, turn);
                    let previous = std::mem::replace(&mut assistant_message_id, next.clone());
                    if events
                        .send(Ok(AgentEvent::Steered {
                            assistant_message_id: Some(previous),
                            next_assistant_message_id: Some(next),
                        }))
                        .await
                        .is_err()
                    {
                        kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                        return Ok(());
                    }
                    done_emitted = false;
                    outstanding_prompts.push(PendingPrompt::new(
                        client.send_command("prompt", prompt_fields(&message.prompt, image_content(&message.attachments))),
                        &message.prompt,
                    ));
                }
                None => steering_open = false,
            },
            _ = turn_control.stop_requested(), if streaming || !done_emitted => {
                stopped = true;
                // Abort only the foreground turn; the persistent native session stays alive.
                tokio::time::timeout(interrupt_grace, client.request("abort", Map::new()))
                    .await.map_err(|_| HarnessError::Protocol("OMP turn abort timed out".into()))??;
            }
            _ = interrupt.cancelled() => {
                // The RPC peer may accept stdin while never answering `abort`,
                // and a successful abort does not exit the persistent RPC host.
                // Bound the acknowledgement phase, then reap the old writer
                // before the engine's longer settlement deadline can expose
                // the session as resumable.
                let _ = tokio::time::timeout(
                    interrupt_grace,
                    client.request("abort", Map::new()),
                )
                .await;
                kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                events
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: Some(session_id.clone()),
                    }))
                    .await
                    .ok();
                return Ok(());
            }
            _ = &mut inactivity => {
                if streaming {
                    tracing::warn!(
                        target: "zeron_harness::omp",
                        process = options.process_label,
                        inactivity_ms = INACTIVITY_LOG_INTERVAL.as_millis() as u64,
                        "RPC turn remains active without frames"
                    );
                }
                inactivity.as_mut().reset(Instant::now() + INACTIVITY_LOG_INTERVAL);
            }
            _ = events.closed() => {
                kill_rpc_process(&mut child, process_group, interrupt_grace).await;
                return Ok(());
            }
        }
        // The run ends only once the mailbox is closed AND the current turn
        // has fully settled: an early mailbox close (engine teardown racing
        // agent_start) must never kill a turn that is still owed its Done.
        if !steering_open && !streaming && done_emitted && outstanding_prompts.is_empty() {
            kill_rpc_process(&mut child, process_group, interrupt_grace).await;
            return Ok(());
        }
    }
        } => result,
        _ = cleanup_interrupt.cancelled() => {
            let _ = tokio::time::timeout(interrupt_grace, client.request("abort", Map::new())).await;
            cancelled_during_startup = true;
            Ok(())
        },
        _ = cleanup_events.closed() => Ok(()),
    };
    kill_rpc_process(&mut child, process_group, interrupt_grace).await;
    if cancelled_during_startup {
        let _ = cleanup_events.send(Ok(AgentEvent::Done { status: DoneStatus::Interrupted,
            result: None, error: None, session_id: request.resume.clone() })).await;
    }
    result
}

/// One-shot command catalog. OMP emits a startup snapshot and may emit richer
/// replacements as asynchronous command providers finish loading. Keep the
/// latest snapshot until the stream settles; the explicit request is the
/// fallback for runtimes that do not push an update.
pub(crate) async fn command_catalog(
    mut process: RpcModeProcess,
    deadline: Duration,
) -> Result<Vec<SlashCommand>, HarnessError> {
    let result = tokio::time::timeout(deadline, async {
        let mut latest = None;
        let mut requested = false;
        loop {
            let received = if latest.is_some() {
                match tokio::time::timeout(COMMAND_CATALOG_SETTLE, process.frames.recv()).await {
                    Ok(received) => received,
                    Err(_) => return Ok(latest.take().expect("catalog snapshot exists")),
                }
            } else {
                process.frames.recv().await
            };
            match received {
                Some(RpcFrame::Frame(frame)) => match frame.get("type").and_then(Value::as_str) {
                    Some("available_commands_update") => {
                        if let Some(commands) = commands_from_frame(&frame) {
                            latest = Some(commands);
                        }
                    }
                    Some("ready") if !requested => {
                        requested = true;
                        process
                            .client
                            .send_command("get_available_commands", Map::new());
                    }
                    Some("response")
                        if frame.get("command").and_then(Value::as_str)
                            == Some("get_available_commands")
                            && frame.get("success").and_then(Value::as_bool) == Some(true) =>
                    {
                        if let Some(commands) = frame.get("data").and_then(commands_from_frame) {
                            latest = Some(commands);
                        }
                    }
                    _ => {}
                },
                Some(RpcFrame::Eof) | None => {
                    if let Some(commands) = latest {
                        return Ok(commands);
                    }
                    return Err(HarnessError::Protocol(crate::crash_message(
                        "OMP RPC command catalog",
                        process.child.try_wait().ok().flatten(),
                        &process.stderr_tail,
                    )));
                }
            }
        }
    })
    .await
    .map_err(|_| HarnessError::Protocol("OMP command catalog timed out".into()))?;
    kill_rpc_process(&mut process.child, process.process_group, Duration::ZERO).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_chunks_round_trip_and_reject_out_of_order_frames() {
        let payload = json!({"type":"response","id":"x","success":true});
        let bytes = serde_json::to_vec(&payload).unwrap();
        let (a, b) = bytes.split_at(bytes.len() / 2);
        let frame = |index, part| json!({
            "type":"rpc_chunk", "chunkId":"r1", "index":index, "count":2,
            "byteLength":bytes.len(), "data":base64::engine::general_purpose::STANDARD.encode(part),
        });
        let mut active = None;
        assert!(assemble_chunk(&mut active, &frame(1, b)).is_none());
        assert!(assemble_chunk(&mut active, &frame(0, a)).is_none());
        assert_eq!(assemble_chunk(&mut active, &frame(1, b)), Some(payload));
    }
}
