//! Crew: OMP's RPC mode (`omp --mode rpc`), ported from Crew's
//! `crates/harness/src/omp/rpc_mode.rs` (Ashler-AI/comet).
//!
//! Newline-delimited JSON over stdio. Unlike upstream's Pi wire, a prompt
//! carries `streamingBehavior: "steer"`, which is legal in every state: idle
//! it starts the next turn, streaming it steers the live one between tool
//! calls. Turns end on `agent_end` (`isTerminal: false` is a retry going on),
//! and their errors ride the assistant `message_end` (`stopReason: "error"`).
//!
//! - Answers to the requests the driver awaits ([`Client::request`]) go
//!   straight back to it; every other frame, other commands' answers
//!   included, goes to the session loop in wire order.
//! - Protocol v2 is negotiated when `ready` offers it: an oversized frame
//!   then arrives as a run of `rpc_chunk` frames, put back together here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, SlashCommand, TodoItem, TodoStatus, ToolCall,
    UserInputAnswer, UserInputQuestion,
};

use crate::{HarnessError, RunControls, SteerMessage, TurnControl};

/// How long OMP has to start: cold capability discovery in a large
/// repository can take most of a minute (Crew's bound).
pub(super) const STARTUP_DEADLINE: Duration = Duration::from_secs(60);
/// A state query or handshake request. OMP answers them at once, even mid-turn.
const STATE_DEADLINE: Duration = Duration::from_secs(15);
/// A turn that streams nothing this long is logged, never ended.
const QUIET_LOG_INTERVAL: Duration = Duration::from_secs(300);
/// Command metadata can change while skills, extensions and MCP prompts
/// finish loading: the newest snapshot is kept until the stream is quiet.
const CATALOG_SETTLE: Duration = Duration::from_secs(2);
/// A stopped turn that never began reports no end of its own: once its
/// abort is answered, this long without an `agent_start` settles it.
const STOP_SETTLE: Duration = Duration::from_secs(1);
/// The largest line read, and the largest chunked frame put back together
/// (OMP's own `maxReassembledFrameBytes`). Past it a frame is dropped, not the run.
const MAX_FRAME: usize = 64 * 1024 * 1024;
/// A tool result's text, as Crew kept it.
const TOOL_OUTPUT_CAP: usize = 16 * 1024;
/// The goal row's call name. Each change gets an id of its own: the engine
/// drops a call that repeats an id from an earlier segment.
const GOAL_CALL_NAME: &str = "OMP goal";

/// `None` once OMP's output has closed: no answer can come any more.
type Pending = Arc<Mutex<Option<HashMap<String, oneshot::Sender<Result<Value, String>>>>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn kind(frame: &Value) -> &str {
    str_field(frame, "type")
}

#[derive(Clone)]
pub(super) struct Client {
    outgoing: mpsc::UnboundedSender<Value>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

impl Client {
    fn command(&self, prefix: &str, kind: &str, mut fields: Map<String, Value>) -> (String, Value) {
        let id = format!("{prefix}{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        fields.insert("id".into(), Value::String(id.clone()));
        fields.insert("type".into(), Value::String(kind.into()));
        (id, Value::Object(fields))
    }

    /// A command whose answer reaches the session loop as a frame.
    pub(super) fn send_command(&self, kind: &str, fields: Map<String, Value>) -> String {
        let (id, frame) = self.command("f", kind, fields);
        let _ = self.outgoing.send(frame);
        id
    }

    /// A frame that is no command: a dialog's answer, a host tool's result.
    pub(super) fn send_frame(&self, frame: Value) {
        let _ = self.outgoing.send(frame);
    }

    /// A command answered straight back to the caller: its `data`.
    pub(super) async fn request(
        &self,
        kind: &str,
        fields: Map<String, Value>,
        deadline: Duration,
    ) -> Result<Value, HarnessError> {
        let (id, frame) = self.command("c", kind, fields);
        let (tx, rx) = oneshot::channel();
        match lock(&self.pending).as_mut() {
            Some(pending) => pending.insert(id.clone(), tx),
            None => return Err(HarnessError::Protocol(format!("OMP exited before answering {kind}"))),
        };
        if self.outgoing.send(frame).is_err() {
            if let Some(pending) = lock(&self.pending).as_mut() {
                pending.remove(&id);
            }
            return Err(HarnessError::Protocol("OMP's input is closed".into()));
        }
        let answer = tokio::time::timeout(deadline, rx).await;
        if let Some(pending) = lock(&self.pending).as_mut() {
            pending.remove(&id);
        }
        match answer {
            Ok(Ok(Ok(data))) => Ok(data),
            Ok(Ok(Err(message))) => Err(HarnessError::Protocol(format!("OMP {kind}: {message}"))),
            Ok(Err(_)) => Err(HarnessError::Protocol(format!("OMP exited before answering {kind}"))),
            Err(_) => Err(HarnessError::Protocol(format!(
                "OMP did not answer {kind} within {}s",
                deadline.as_secs()
            ))),
        }
    }
}

/// OMP's stdin and stdout: the client that writes, the frames that were read.
pub(super) fn transport(
    input: impl AsyncWrite + Unpin + Send + 'static,
    output: impl AsyncRead + Unpin + Send + 'static,
) -> (Client, mpsc::Receiver<Value>, [JoinHandle<()>; 2]) {
    let (outgoing, to_write) = mpsc::unbounded_channel();
    let (frames_tx, frames) = mpsc::channel(256);
    let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
    let writer = tokio::spawn(write_frames(input, to_write));
    let reader = tokio::spawn(read_frames(output, pending.clone(), frames_tx, MAX_FRAME));
    let client = Client {
        outgoing,
        pending,
        next_id: Arc::new(AtomicU64::new(1)),
    };
    (client, frames, [reader, writer])
}

async fn write_frames(mut input: impl AsyncWrite + Unpin, mut to_write: mpsc::UnboundedReceiver<Value>) {
    while let Some(frame) = to_write.recv().await {
        let mut bytes = serde_json::to_vec(&frame).expect("a JSON value serializes");
        bytes.push(b'\n');
        let written = async {
            input.write_all(&bytes).await?;
            input.flush().await
        }
        .await;
        if let Err(error) = written {
            // OMP has gone: its output's end reports it.
            tracing::debug!(target: "zeron_harness::crew_omp", %error, "OMP stdin write failed");
            break;
        }
    }
}

async fn read_frames(
    output: impl AsyncRead + Unpin,
    pending: Pending,
    frames: mpsc::Sender<Value>,
    max_frame: usize,
) {
    let mut reader = BufReader::new(output);
    let mut line = Vec::new();
    // Inside an oversized line: discard through its newline.
    let mut skipping = false;
    let mut chunks = None;
    loop {
        let buffer = match reader.fill_buf().await {
            Ok([]) => break,
            Ok(buffer) => buffer,
            Err(error) => {
                tracing::debug!(target: "zeron_harness::crew_omp", %error, "OMP stdout read failed");
                break;
            }
        };
        let count = buffer.iter().position(|b| *b == b'\n').map_or(buffer.len(), |at| at + 1);
        let complete = buffer[count - 1] == b'\n';
        if skipping {
            reader.consume(count);
            skipping = !complete;
            continue;
        }
        if line.len() + count > max_frame {
            tracing::warn!(target: "zeron_harness::crew_omp", "OMP frame over {} MiB dropped", max_frame >> 20);
            line.clear();
            reader.consume(count);
            skipping = !complete;
            continue;
        }
        line.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if !complete {
            continue;
        }
        let parsed = parse_frame(line.trim_ascii());
        if parsed.is_none() && !line.trim_ascii().is_empty() {
            // A dependency's stray console output is no reason to end the run.
            tracing::debug!(target: "zeron_harness::crew_omp", "unparseable OMP line skipped");
        }
        line.clear();
        let Some(mut frame) = parsed else { continue };
        if kind(&frame) == "rpc_chunk" {
            match assemble_chunk(&mut chunks, &frame, max_frame) {
                Some(whole) => frame = whole,
                None => continue,
            }
        }
        if kind(&frame) == "response" && resolve_pending(&pending, &frame) {
            continue;
        }
        if frames.send(frame).await.is_err() {
            break;
        }
    }
    // Every awaited request fails now; the session loop sees the channel close.
    lock(&pending).take();
}

/// Hands an awaited request its answer; false for the session loop's frames.
fn resolve_pending(pending: &Pending, frame: &Value) -> bool {
    let Some(id) = frame.get("id").and_then(Value::as_str) else {
        return false;
    };
    let Some(sender) = lock(pending).as_mut().and_then(|pending| pending.remove(id)) else {
        return false;
    };
    let answer = if frame.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(frame.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(frame.get("error").and_then(Value::as_str).unwrap_or("command failed").to_owned())
    };
    let _ = sender.send(answer);
    true
}

/// An `rpc_chunk` run being put back together (protocol v2).
struct Chunks {
    chunk_id: String,
    count: u64,
    byte_length: u64,
    next_index: u64,
    bytes: Vec<u8>,
}

/// Folds one chunk in; the whole frame once its run ends. A run that breaks
/// sequence, decodes badly or overruns its declared length is dropped.
fn assemble_chunk(active: &mut Option<Chunks>, frame: &Value, max_frame: usize) -> Option<Value> {
    let chunk_id = frame.get("chunkId").and_then(Value::as_str)?;
    let index = frame.get("index").and_then(Value::as_u64)?;
    let count = frame.get("count").and_then(Value::as_u64)?;
    let byte_length = frame.get("byteLength").and_then(Value::as_u64)?;
    let data = frame.get("data").and_then(Value::as_str)?;
    if index == 0 {
        if byte_length > max_frame as u64 {
            tracing::warn!(target: "zeron_harness::crew_omp", chunk_id, byte_length, "OMP chunked frame too large; dropped");
            *active = None;
            return None;
        }
        *active = Some(Chunks {
            chunk_id: chunk_id.to_owned(),
            count,
            byte_length,
            next_index: 0,
            bytes: Vec::new(),
        });
    }
    let chunks = active.as_mut()?;
    if chunks.chunk_id != chunk_id || chunks.count != count || chunks.byte_length != byte_length || chunks.next_index != index {
        tracing::warn!(target: "zeron_harness::crew_omp", chunk_id, "OMP chunk out of sequence; dropped");
        *active = None;
        return None;
    }
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(data) else {
        tracing::warn!(target: "zeron_harness::crew_omp", chunk_id, "OMP chunk is not base64; dropped");
        *active = None;
        return None;
    };
    chunks.bytes.extend_from_slice(&decoded);
    chunks.next_index += 1;
    if chunks.bytes.len() as u64 > chunks.byte_length {
        tracing::warn!(target: "zeron_harness::crew_omp", chunk_id, "OMP chunks overran their length; dropped");
        *active = None;
        return None;
    }
    if chunks.next_index < chunks.count {
        return None;
    }
    let chunks = active.take()?;
    if chunks.bytes.len() as u64 != chunks.byte_length {
        tracing::warn!(target: "zeron_harness::crew_omp", chunk_id, "OMP chunks fell short of their length; dropped");
        return None;
    }
    let whole = parse_frame(&chunks.bytes);
    if whole.is_none() {
        tracing::warn!(target: "zeron_harness::crew_omp", chunk_id, "OMP chunked frame is not JSON; dropped");
    }
    whole
}

/// A frame, tolerating the one thing JS emits that serde_json refuses: a lone
/// UTF-16 surrogate escape (a string sliced mid-pair), which becomes U+FFFD
/// instead of costing the frame. As upstream's Pi transport reads its wire.
fn parse_frame(bytes: &[u8]) -> Option<Value> {
    serde_json::from_slice(bytes).ok().or_else(|| {
        let repaired = replace_lone_surrogates(bytes)?;
        serde_json::from_slice(&repaired).ok()
    })
}

/// `bytes` with each unpaired `\uD800`–`\uDFFF` escape made `�`; `None`
/// when there was none.
fn replace_lone_surrogates(bytes: &[u8]) -> Option<Vec<u8>> {
    fn unit(bytes: &[u8], at: usize) -> Option<u16> {
        if bytes.get(at..at + 2)? != b"\\u" {
            return None;
        }
        let hex = bytes.get(at + 2..at + 6)?;
        if !hex.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        u16::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut changed = false;
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'\\' {
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        match unit(bytes, at) {
            Some(0xD800..=0xDBFF) if matches!(unit(bytes, at + 6), Some(0xDC00..=0xDFFF)) => {
                out.extend_from_slice(&bytes[at..at + 12]);
                at += 12;
            }
            Some(0xD800..=0xDFFF) => {
                out.extend_from_slice(b"\\ufffd");
                changed = true;
                at += 6;
            }
            Some(_) => {
                out.extend_from_slice(&bytes[at..at + 6]);
                at += 6;
            }
            // Any other escape whole, so an escaped backslash stays literal.
            None => {
                let end = (at + 2).min(bytes.len());
                out.extend_from_slice(&bytes[at..end]);
                at = end;
            }
        }
    }
    changed.then_some(out)
}

// ---------------------------------------------------------------------------
// OMP's frames as upstream's events
// ---------------------------------------------------------------------------

fn text_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

/// A tool's start as the call kinds the transcript renders natively; any
/// other tool is shown under its intent, else its name.
fn tool_call(frame: &Value) -> Option<AgentEvent> {
    let id = frame.get("toolCallId")?.as_str()?.to_owned();
    let name = str_field(frame, "toolName");
    let args = frame.get("args").cloned().unwrap_or(Value::Null);
    let arg = |key: &str| text_arg(&args, key);
    let call = match name {
        "bash" => arg("command").map(|command| ToolCall::Exec { command }),
        "read" => arg("path").map(|path| ToolCall::ReadFile { path }),
        "write" => arg("path").map(|path| ToolCall::WriteFile {
            path,
            content: arg("content"),
        }),
        "edit" => arg("path").map(|path| ToolCall::EditFile {
            path,
            old_string: arg("oldText").or_else(|| arg("old_string")),
            new_string: arg("newText").or_else(|| arg("new_string")),
        }),
        "grep" => arg("pattern").map(|pattern| ToolCall::Search {
            pattern,
            path: arg("path"),
        }),
        "glob" | "find" => arg("pattern").map(|pattern| ToolCall::Glob { pattern }),
        "web_search" => arg("query").map(|query| ToolCall::WebSearch { query }),
        // A subagent: upstream's spawn chip.
        "task" => Some(ToolCall::Unknown {
            name: "Agent".into(),
            input: Some(args.clone()),
        }),
        _ => None,
    }
    .unwrap_or_else(|| ToolCall::Unknown {
        name: frame
            .get("intent")
            .and_then(Value::as_str)
            .filter(|intent| !intent.trim().is_empty())
            .unwrap_or(if name.is_empty() { "OMP tool" } else { name })
            .to_owned(),
        input: (!args.is_null()).then_some(args),
    });
    Some(AgentEvent::ToolCall { id, call })
}

fn result_text(result: &Value) -> Option<String> {
    if let Some(text) = result.as_str() {
        return (!text.is_empty()).then(|| text.to_owned());
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
    (!result.is_null()).then(|| serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string()))
}

fn tool_result(frame: &Value) -> AgentEvent {
    AgentEvent::ToolResult {
        id: frame.get("toolCallId").and_then(Value::as_str).unwrap_or("omp-tool").to_owned(),
        is_error: frame.get("isError").and_then(Value::as_bool) == Some(true),
        output: frame.get("result").and_then(result_text).map(|mut output| {
            let mut end = output.len().min(TOOL_OUTPUT_CAP);
            while !output.is_char_boundary(end) {
                end -= 1;
            }
            output.truncate(end);
            output
        }),
        diff: None,
        preview_paths: Vec::new(),
        image_payloads: Vec::new(),
        exit_code: None,
    }
}

fn assistant_delta(frame: &Value) -> Option<AgentEvent> {
    let event = frame.get("assistantMessageEvent")?;
    let delta = str_field(event, "delta");
    match str_field(event, "type") {
        "text_delta" if !delta.is_empty() => Some(AgentEvent::TextDelta { text: delta.into() }),
        "thinking_delta" if !delta.is_empty() => Some(AgentEvent::ReasoningDelta { text: delta.into() }),
        _ => None,
    }
}

/// A local command's output (`/mcp`, …), indented as a block.
fn command_output(frame: &Value) -> Option<AgentEvent> {
    frame
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| AgentEvent::TextDelta {
            text: format!("    {}", text.replace('\n', "\n    ")),
        })
}

/// The error an assistant message ended on (`errorMessage` is the provider's).
fn assistant_error(message: &Value) -> Option<String> {
    (str_field(message, "stopReason") == "error").then(|| {
        message
            .get("errorMessage")
            .and_then(Value::as_str)
            .unwrap_or("OMP reported a model error")
            .to_owned()
    })
}

/// A message's usage, and what the context holds after it.
fn usage(message: &Value, window: Option<u64>) -> Vec<AgentEvent> {
    let Some(usage) = message.get("usage").filter(|usage| usage.is_object()) else {
        return Vec::new();
    };
    let read = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let input_tokens = read("input") + read("cacheRead") + read("cacheWrite");
    let output_tokens = read("output");
    if input_tokens == 0 && output_tokens == 0 {
        return Vec::new();
    }
    vec![
        AgentEvent::Usage {
            input_tokens,
            output_tokens,
        },
        AgentEvent::ContextUsage {
            tokens: Some(input_tokens + output_tokens),
            window,
        },
    ]
}

fn todo_items(state: &Value) -> Vec<TodoItem> {
    state
        .get("todoPhases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|phase| phase.get("tasks").and_then(Value::as_array))
        .flatten()
        .filter_map(|task| {
            let text = task.get("content").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty())?;
            Some(TodoItem::new(text, TodoStatus::parse(str_field(task, "status"))))
        })
        .collect()
}

/// A resolved chip: upstream's live plan, or the goal row.
fn chip(id: String, call: ToolCall) -> [AgentEvent; 2] {
    [
        AgentEvent::ToolCall { id: id.clone(), call },
        AgentEvent::ToolResult {
            id,
            is_error: false,
            output: None,
            diff: None,
            preview_paths: Vec::new(),
            image_payloads: Vec::new(),
            exit_code: None,
        },
    ]
}

fn plan(state: &Value) -> [AgentEvent; 2] {
    chip(zeron_proto::LIVE_PLAN_TOOL_ID.into(), ToolCall::Todo { items: todo_items(state) })
}

fn goal(goal: Value, state: Option<Value>) -> [AgentEvent; 2] {
    chip(
        format!("omp-goal-{}", uuid::Uuid::new_v4()),
        ToolCall::Unknown {
            name: GOAL_CALL_NAME.into(),
            input: Some(json!({ "goal": goal, "state": state })),
        },
    )
}

/// The goal in a `get_state` answer (its `goalMode`), if the session has one.
fn goal_in_state(state: &Value) -> Option<(Value, Value)> {
    let mode = state.get("goalMode").or_else(|| state.get("goal_mode"))?.clone();
    Some((mode.get("goal").cloned().unwrap_or(Value::Null), mode))
}

fn is_goal_command(prompt: &str) -> bool {
    prompt
        .trim_start()
        .strip_prefix("/goal")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// A command list (`available_commands_update`, `get_available_commands`),
/// with `/goal`, which OMP handles but may not list.
fn commands(frame: &Value) -> Option<Vec<SlashCommand>> {
    let mut commands: Vec<_> = frame
        .get("commands")?
        .as_array()?
        .iter()
        .filter_map(|command| {
            Some(SlashCommand {
                name: command.get("name")?.as_str()?.to_owned(),
                description: str_field(command, "description").to_owned(),
                input_hint: command.pointer("/input/hint").and_then(Value::as_str).map(str::to_owned),
            })
        })
        .collect();
    if !commands.iter().any(|command| command.name == "goal") {
        commands.push(SlashCommand {
            name: "goal".into(),
            description: "Manage this session's goal".into(),
            input_hint: Some("goal | status | drop".into()),
        });
    }
    Some(commands)
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

type Input = dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync;

/// An extension's dialogs (`extension_ui_request`) as upstream's questions:
/// `confirm` and `select` as choices, `input` and `editor` as free text.
/// The rest (`notify`, `setStatus`, …) need no answer.
#[derive(Default)]
struct Dialogs {
    tasks: tokio::task::JoinSet<()>,
    /// The dialogs OMP still waits on; whoever takes one out answers it, once.
    open: Arc<Mutex<Vec<Value>>>,
}

impl Dialogs {
    fn request(&mut self, client: &Client, input: &Arc<Input>, frame: &Value) {
        let method = str_field(frame, "method").to_owned();
        if !matches!(method.as_str(), "confirm" | "select" | "input" | "editor") {
            return;
        }
        while self.tasks.try_join_next().is_some() {}
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let cancelled = json!({"type": "extension_ui_response", "id": id, "cancelled": true});
        let title = Some(str_field(frame, "title")).filter(|title| !title.trim().is_empty()).unwrap_or("OMP");
        let message = str_field(frame, "message");
        let options: Vec<String> = match method.as_str() {
            "confirm" => vec!["Continue".into(), "Cancel".into()],
            "select" => frame
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| option.as_str().or_else(|| option.get("label").and_then(Value::as_str)))
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        let free_text = matches!(method.as_str(), "input" | "editor");
        let question = UserInputQuestion {
            id: uuid::Uuid::new_v4().to_string(),
            header: title.to_owned(),
            question: if method == "confirm" && message.is_empty() { "Continue?".into() } else { message.to_owned() },
            options: options.clone(),
            multi_select: false,
            prefill: frame.get("prefill").and_then(Value::as_str).filter(|_| free_text).map(str::to_owned),
            multiline: method == "editor",
            note: None,
        };
        // OMP's `timeout` is in milliseconds: 0 is none, and a negative one
        // has run out already. One too large for a Duration is none.
        let timeout = match frame.get("timeout").and_then(Value::as_f64) {
            Some(ms) if ms < 0.0 => {
                client.send_frame(cancelled);
                return;
            }
            Some(ms) if ms > 0.0 => Duration::try_from_secs_f64(ms / 1000.0).ok(),
            _ => None,
        };
        let question_id = question.id.clone();
        let answer = input(vec![question]);
        lock(&self.open).push(id.clone());
        let open = Arc::clone(&self.open);
        let client = client.clone();
        self.tasks.spawn(async move {
            let expired = async {
                match timeout {
                    Some(timeout) => tokio::time::sleep(timeout).await,
                    None => std::future::pending().await,
                }
            };
            let answers = tokio::select! {
                answers = answer => answers.unwrap_or_default(),
                () = expired => Vec::new(),
            };
            let label = answers
                .iter()
                .find(|answer| answer.question_id == question_id)
                .or(answers.first())
                .and_then(|answer| answer.labels.first())
                .cloned();
            let mut response = json!({"type": "extension_ui_response", "id": id});
            match (method.as_str(), label) {
                ("confirm", Some(label)) if options.contains(&label) => response["confirmed"] = json!(label == "Continue"),
                ("select", Some(label)) if options.contains(&label) => response["value"] = json!(label),
                ("input" | "editor", Some(label)) => response["value"] = json!(label),
                _ => response["cancelled"] = json!(true),
            }
            let mut open = lock(&open);
            if let Some(index) = open.iter().position(|pending| *pending == id) {
                open.remove(index);
                client.send_frame(response);
            }
        });
    }

    /// Close every open dialog: an extension awaiting one would wait forever.
    fn cancel(&mut self, client: &Client) {
        self.tasks.abort_all();
        for id in std::mem::take(&mut *lock(&self.open)) {
            client.send_frame(json!({"type": "extension_ui_response", "id": id, "cancelled": true}));
        }
    }
}

// ---------------------------------------------------------------------------
// The process
// ---------------------------------------------------------------------------

/// One `omp --mode rpc` child, its wire and its stderr tail.
pub(super) struct Process {
    pub(super) child: crate::process::owned::Child,
    pub(super) client: Client,
    pub(super) frames: mpsc::Receiver<Value>,
    pub(super) tail: crate::StderrTail,
    tasks: Vec<JoinHandle<()>>,
    /// The run's own files (its config overlay, the gateway extension): kept
    /// until the child has gone, and dropped after it.
    _scratch: Option<crate::scratch::ScratchDir>,
}

impl Drop for Process {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Process {
    /// Wire up a child spawned with piped stdio in a process group of its own.
    pub(super) fn new(
        child: crate::process::Child,
        scratch: Option<crate::scratch::ScratchDir>,
    ) -> Result<Self, HarnessError> {
        let mut child = crate::process::owned::Child::new(child);
        let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            return Err(HarnessError::Protocol("OMP has no stdio".into()));
        };
        let tail = crate::StderrTail::default();
        let stderr_tail = tail.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = crate::lossy_lines::lossy_lines(stderr);
            while let Ok(Some(line)) = lines.next_line().await {
                stderr_tail.push(&line);
            }
            stderr_tail.close();
        });
        let (client, frames, [reader, writer]) = transport(stdin, stdout);
        Ok(Self {
            child,
            client,
            frames,
            tail,
            tasks: vec![reader, writer, stderr_task],
            _scratch: scratch,
        })
    }
}

// ---------------------------------------------------------------------------
// A run
// ---------------------------------------------------------------------------

pub(super) struct Options {
    /// The session header id of the journal given to `--resume`: OMP must
    /// report that very session, or the run fails.
    pub(super) expected_resume: Option<String>,
    /// Why an asked-for resume could not be honored: shown once the fresh
    /// session has started.
    pub(super) lost_context: Option<String>,
    /// How long an interrupt's `abort` may take before the process is ended.
    pub(super) interrupt_grace: Duration,
    pub(super) kill_grace: Duration,
    /// Attachments inline only what fits Agent Auth's 8 MiB request.
    pub(super) managed: bool,
}

/// The most of the attachments a managed prompt inlines (base64 makes 4 MiB
/// about 5.3): the rest ride as the path references the prompt already has.
const MANAGED_INLINE_IMAGES: u64 = 4 * 1024 * 1024;

struct PendingPrompt {
    id: String,
    /// `/goal …` changes the goal without always saying so over RPC: the
    /// state is read again once OMP has taken the prompt.
    refresh_goal: bool,
}

enum Flow {
    Continue,
    /// The run is over (an errored turn): its process has been reaped.
    Ended,
}

enum Started {
    Ready(Result<(), HarnessError>),
    Interrupted,
    Closed,
}

enum Wake {
    Frame(Option<Value>),
    Steer(Option<SteerMessage>),
    Stop,
    Interrupt,
    StateDeadline,
    StopSettle,
    Quiet,
    Closed,
}

struct Session {
    process: Process,
    events: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    input: Arc<Input>,
    dialogs: Dialogs,
    turn: TurnControl,
    request: RunRequest,
    options: Options,
    session_id: String,
    /// The model as `SessionStarted` names it: `{provider}/{id}`.
    model: String,
    nonce: uuid::Uuid,
    segment: u64,
    assistant: String,
    window: Option<u64>,
    /// Inside a turn: from `agent_start` to its terminal `agent_end`.
    streaming: bool,
    /// A `Done` is owed: a prompt of ours, or a turn OMP began itself, is open.
    owed: bool,
    /// The open turn was stopped ([`TurnControl::stop_turn`]): it ends Interrupted.
    stopped: bool,
    turn_error: Option<String>,
    prompts: Vec<PendingPrompt>,
    todo_request: Option<String>,
    goal_request: Option<String>,
    state_deadline: Option<Instant>,
    abort_request: Option<String>,
    stop_settle: Option<Instant>,
}

/// Drive one persistent RPC session: its first prompt, then each steer, until
/// the steering mailbox closes with no turn open, the run is interrupted, its
/// events are no longer read, or OMP fails. The execution lease is held until
/// the child has been reaped.
pub(super) async fn run(
    process: Process,
    request: RunRequest,
    controls: RunControls,
    events: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    options: Options,
) {
    let RunControls {
        realtime: _,
        execution_lease: _lease,
        request_input,
        mut steering,
        interrupt,
        turn,
    } = controls;
    let mut session = Session {
        process,
        events,
        input: Arc::from(request_input),
        dialogs: Dialogs::default(),
        turn,
        request,
        options,
        session_id: String::new(),
        model: String::new(),
        nonce: uuid::Uuid::new_v4(),
        segment: 0,
        assistant: String::new(),
        window: None,
        streaming: false,
        owed: true,
        stopped: false,
        turn_error: None,
        prompts: Vec::new(),
        todo_request: None,
        goal_request: None,
        state_deadline: None,
        abort_request: None,
        stop_settle: None,
    };
    session.drive(&mut steering, &interrupt).await;
}

impl Session {
    async fn drive(&mut self, steering: &mut mpsc::Receiver<SteerMessage>, interrupt: &crate::CancellationToken) {
        let events = self.events.clone();
        let started = tokio::select! {
            started = self.start() => Started::Ready(started),
            () = interrupt.cancelled() => Started::Interrupted,
            () = events.closed() => Started::Closed,
        };
        let result = match started {
            Started::Ready(Ok(())) => self.serve(steering, interrupt).await,
            Started::Ready(Err(error)) => Err(error),
            Started::Interrupted => {
                self.interrupt().await;
                return;
            }
            Started::Closed => {
                self.shutdown().await;
                return;
            }
        };
        if let Err(error) = result {
            self.fail(error).await;
        }
    }

    async fn emit(&self, event: AgentEvent) -> Result<(), HarnessError> {
        self.events
            .send(Ok(event))
            .await
            .map_err(|_| HarnessError::Protocol("OMP's events are no longer read".into()))
    }

    fn session(&self) -> Option<String> {
        Some(self.session_id.clone())
            .filter(|id| !id.is_empty())
            .or_else(|| self.options.expected_resume.clone())
    }

    /// Until the engine has taken every event sent so far, for a second at
    /// most: it reads them in order, so it has then had the last turn's Done.
    async fn drained(&self) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.events.capacity() < self.events.max_capacity() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The session, as its start or a turn OMP began itself announces it.
    fn session_started(&self) -> AgentEvent {
        AgentEvent::SessionStarted {
            harness: HarnessId::Omp,
            model: self.model.clone(),
            tools: Vec::new(),
            cwd: self.request.cwd.clone(),
            session_id: self.session_id.clone(),
            assistant_message_id: self.assistant.clone(),
        }
    }

    fn next_assistant(&mut self) -> String {
        self.segment += 1;
        std::mem::replace(&mut self.assistant, format!("omp-rpc-{}-{}-{}", self.session_id, self.nonce, self.segment))
    }

    async fn images(&self, paths: &[String]) -> Value {
        if self.options.managed {
            crate::pi::load_images_capped(paths, MANAGED_INLINE_IMAGES, MANAGED_INLINE_IMAGES).await
        } else {
            crate::pi::load_images(paths).await
        }
    }

    fn prompt(&mut self, message: &str, images: Value) {
        let mut fields = Map::new();
        fields.insert("message".into(), Value::String(message.into()));
        if images.as_array().is_some_and(|images| !images.is_empty()) {
            fields.insert("images".into(), images);
        }
        // Idle it starts the next turn; streaming it steers the live one.
        fields.insert("streamingBehavior".into(), Value::String("steer".into()));
        let id = self.process.client.send_command("prompt", fields);
        self.prompts.push(PendingPrompt {
            id,
            refresh_goal: is_goal_command(message),
        });
    }

    fn refresh_state(&mut self) -> String {
        self.state_deadline = Some(Instant::now() + STATE_DEADLINE);
        self.process.client.send_command("get_state", Map::new())
    }

    /// `ready` (always OMP's first frame), v2 framing when offered, then the
    /// session's identity from `get_state`, and the first prompt.
    async fn start(&mut self) -> Result<(), HarnessError> {
        let frames = &mut self.process.frames;
        let ready = tokio::time::timeout(STARTUP_DEADLINE, async {
            while let Some(frame) = frames.recv().await {
                if kind(&frame) == "ready" {
                    return Some(frame);
                }
            }
            None
        })
        .await
        .map_err(|_| HarnessError::Protocol(format!("OMP did not start within {}s", STARTUP_DEADLINE.as_secs())))?
        .ok_or_else(|| HarnessError::Protocol("OMP exited before it was ready".into()))?;
        let client = self.process.client.clone();
        let offers_v2 = ready
            .get("supportedProtocolVersions")
            .and_then(Value::as_array)
            .is_some_and(|versions| versions.iter().any(|version| version.as_u64() == Some(2)));
        if offers_v2 {
            let mut fields = Map::new();
            fields.insert("protocolVersion".into(), json!(2));
            if let Err(error) = client.request("negotiate_protocol", fields, STATE_DEADLINE).await {
                tracing::warn!(target: "zeron_harness::crew_omp", %error, "OMP protocol v2 refused; staying on v1");
            }
        }
        let state = client.request("get_state", Map::new(), STATE_DEADLINE).await?;
        let session_id = state
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| HarnessError::Protocol("OMP's get_state named no session".into()))?;
        if let Some(expected) = self.options.expected_resume.as_deref()
            && session_id != expected
        {
            return Err(HarnessError::Protocol(format!("OMP resumed session {session_id} instead of {expected}")));
        }
        self.session_id = session_id.to_owned();
        let model = state.get("model").unwrap_or(&Value::Null);
        self.window = model.get("contextWindow").and_then(Value::as_u64);
        // As the chat names it: a managed route's provider without its `comet-`.
        let provider = str_field(model, "provider");
        let provider = provider.strip_prefix("comet-").unwrap_or(provider);
        self.model = match (provider, str_field(model, "id")) {
            (_, "") => self.request.model.clone().unwrap_or_else(|| "default".into()),
            ("", id) => id.to_owned(),
            (provider, id) => format!("{provider}/{id}"),
        };
        self.next_assistant();
        self.emit(self.session_started()).await?;
        if let Some(message) = self.options.lost_context.take() {
            self.emit(AgentEvent::Error { message }).await?;
        }
        if let Some((current, mode)) = goal_in_state(&state).filter(|(current, _)| !current.is_null()) {
            for event in goal(current, Some(mode)) {
                self.emit(event).await?;
            }
        }
        let images = self.images(&self.request.attachments).await;
        let prompt = self.request.prompt.clone();
        self.prompt(&prompt, images);
        Ok(())
    }

    async fn serve(
        &mut self,
        steering: &mut mpsc::Receiver<SteerMessage>,
        interrupt: &crate::CancellationToken,
    ) -> Result<(), HarnessError> {
        let mut steering_open = true;
        let quiet = tokio::time::sleep(QUIET_LOG_INTERVAL);
        tokio::pin!(quiet);
        loop {
            let state_deadline = self.state_deadline;
            let stop_settle = self.stop_settle;
            // OMP's frames come before what the host asks, so a stop meets the
            // turn as it now stands; a stop before a steer, so one sent with
            // no turn open is spent before the steer's turn begins.
            let wake = tokio::select! {
                biased;
                () = interrupt.cancelled() => Wake::Interrupt,
                () = self.events.closed() => Wake::Closed,
                frame = self.process.frames.recv() => Wake::Frame(frame),
                () = self.turn.stop_requested() => Wake::Stop,
                steer = steering.recv(), if steering_open => Wake::Steer(steer),
                () = tokio::time::sleep_until(state_deadline.unwrap_or_else(Instant::now)), if state_deadline.is_some() => Wake::StateDeadline,
                () = tokio::time::sleep_until(stop_settle.unwrap_or_else(Instant::now)), if stop_settle.is_some() => Wake::StopSettle,
                () = &mut quiet => Wake::Quiet,
            };
            match wake {
                Wake::Interrupt => {
                    self.interrupt().await;
                    return Ok(());
                }
                Wake::Closed => {
                    self.shutdown().await;
                    return Ok(());
                }
                // The child never closes its output while its input is open.
                Wake::Frame(None) => return Err(HarnessError::Protocol("OMP closed its output".into())),
                Wake::Frame(Some(frame)) => {
                    quiet.as_mut().reset(Instant::now() + QUIET_LOG_INTERVAL);
                    if let Flow::Ended = self.frame(frame).await? {
                        return Ok(());
                    }
                }
                Wake::Steer(Some(steer)) => self.steer(steer).await?,
                Wake::Steer(None) => steering_open = false,
                Wake::Stop => self.stop(),
                Wake::StateDeadline => {
                    self.state_deadline = None;
                    // Both go: one left waiting would hold the run open.
                    let (todo, goal) = (self.todo_request.take(), self.goal_request.take());
                    if todo.is_some() || goal.is_some() {
                        tracing::warn!(target: "zeron_harness::crew_omp", "OMP state refresh timed out");
                    }
                }
                Wake::StopSettle => {
                    self.stop_settle = None;
                    if self.owed && !self.streaming {
                        self.done(DoneStatus::Interrupted, None).await?;
                    }
                }
                Wake::Quiet => {
                    if self.streaming {
                        tracing::warn!(target: "zeron_harness::crew_omp", "OMP turn has streamed nothing for {}s", QUIET_LOG_INTERVAL.as_secs());
                    }
                    quiet.as_mut().reset(Instant::now() + QUIET_LOG_INTERVAL);
                }
            }
            // The mailbox closing (the engine letting the run go) never cuts
            // short a turn still owed its Done.
            if !steering_open
                && !self.owed
                && !self.streaming
                && self.prompts.is_empty()
                && self.todo_request.is_none()
                && self.goal_request.is_none()
            {
                self.shutdown().await;
                return Ok(());
            }
        }
    }

    async fn done(&mut self, status: DoneStatus, error: Option<String>) -> Result<(), HarnessError> {
        self.owed = false;
        self.stopped = false;
        self.stop_settle = None;
        self.dialogs.cancel(&self.process.client);
        self.emit(AgentEvent::Done {
            status,
            result: None,
            error,
            session_id: self.session(),
        })
        .await
    }

    async fn frame(&mut self, frame: Value) -> Result<Flow, HarnessError> {
        let is_assistant = |frame: &Value| frame.pointer("/message/role").and_then(Value::as_str) == Some("assistant");
        match kind(&frame) {
            "agent_start" => {
                // A turn nobody sent (OMP woke itself, or took a steer that
                // missed the last turn's end): the engine reopens a parked
                // session at once, before this turn's first words. It must
                // have taken the last turn's Done first: a wake it hears while
                // still in that turn is spent there, and this turn's opening
                // output then falls in the parked session's quiet second.
                if !self.owed {
                    self.drained().await;
                    self.turn.note_wake();
                    self.next_assistant();
                    self.emit(self.session_started()).await?;
                }
                self.streaming = true;
                self.owed = true;
                // It began: its own end settles a stop.
                self.stop_settle = None;
            }
            "message_start" if is_assistant(&frame) => self.turn_error = None,
            "message_update" => {
                if let Some(event) = assistant_delta(&frame) {
                    self.emit(event).await?;
                }
            }
            "command_output" => {
                if let Some(event) = command_output(&frame) {
                    self.emit(event).await?;
                }
            }
            "message_end" if is_assistant(&frame) => {
                let message = &frame["message"];
                self.turn_error = assistant_error(message);
                for event in usage(message, self.window) {
                    self.emit(event).await?;
                }
            }
            "tool_execution_start" => {
                if let Some(event) = tool_call(&frame) {
                    self.emit(event).await?;
                }
            }
            "tool_execution_end" => {
                self.emit(tool_result(&frame)).await?;
                let todo = str_field(&frame, "toolName") == "todo" && frame.get("isError").and_then(Value::as_bool) != Some(true);
                if todo && self.todo_request.is_none() {
                    self.todo_request = Some(self.refresh_state());
                }
            }
            "goal_updated" => {
                for event in goal(frame.get("goal").cloned().unwrap_or(Value::Null), frame.get("state").cloned()) {
                    self.emit(event).await?;
                }
            }
            "available_commands_update" => {
                if let Some(commands) = commands(&frame) {
                    self.emit(AgentEvent::AvailableCommands { commands }).await?;
                }
            }
            // An extension's failure, not the turn's: shown, and the turn goes on.
            "extension_error" => {
                self.emit(AgentEvent::Error {
                    message: format!("OMP extension: {}", str_field(&frame, "error")),
                })
                .await?;
            }
            "agent_end" => {
                // A retry continues the same turn.
                if frame.get("isTerminal").and_then(Value::as_bool) == Some(false) {
                    return Ok(Flow::Continue);
                }
                self.streaming = false;
                let status = if self.stopped {
                    DoneStatus::Interrupted
                } else if self.turn_error.is_some() {
                    DoneStatus::Errored
                } else {
                    DoneStatus::Completed
                };
                let error = self.turn_error.take();
                if status == DoneStatus::Errored {
                    // A failed turn ends the run: its process is reaped before
                    // Done, so the next message's resume never meets it. Done
                    // carries the one visible error.
                    self.shutdown().await;
                    self.done(status, error).await?;
                    return Ok(Flow::Ended);
                }
                self.done(status, error).await?;
            }
            // A prompt OMP settled without the agent (a local command).
            "prompt_result" => {
                if self.owed && !self.streaming {
                    self.done(DoneStatus::Completed, None).await?;
                }
            }
            "response" => self.response(&frame).await?,
            "extension_ui_request" => {
                let input = Arc::clone(&self.input);
                self.dialogs.request(&self.process.client, &input, &frame);
            }
            "host_tool_call" => {
                if let Some(id) = frame.get("id").and_then(Value::as_str) {
                    self.process.client.send_frame(json!({
                        "type": "host_tool_result",
                        "id": id,
                        "isError": true,
                        "result": { "content": [{ "type": "text", "text": "Crew registers no host tools" }] },
                    }));
                }
            }
            "host_uri_request" => {
                if let Some(id) = frame.get("id").and_then(Value::as_str) {
                    self.process.client.send_frame(json!({
                        "type": "host_uri_result",
                        "id": id,
                        "isError": true,
                        "error": "Crew registers no host URI schemes",
                    }));
                }
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    async fn response(&mut self, frame: &Value) -> Result<(), HarnessError> {
        let id = str_field(frame, "id");
        let success = frame.get("success").and_then(Value::as_bool) == Some(true);
        if self.abort_request.as_deref() == Some(id) {
            self.abort_request = None;
            if !success {
                tracing::warn!(target: "zeron_harness::crew_omp", error = str_field(frame, "error"), "OMP refused an abort");
            }
            if self.owed && !self.streaming {
                self.stop_settle.get_or_insert_with(|| Instant::now() + STOP_SETTLE);
            }
            return Ok(());
        }
        let todo_refresh = self.todo_request.as_deref() == Some(id);
        let goal_refresh = self.goal_request.as_deref() == Some(id);
        if todo_refresh || goal_refresh {
            if todo_refresh {
                self.todo_request = None;
            }
            if goal_refresh {
                self.goal_request = None;
            }
            if self.todo_request.is_none() && self.goal_request.is_none() {
                self.state_deadline = None;
            }
            if !success {
                tracing::warn!(target: "zeron_harness::crew_omp", "OMP state refresh failed");
                return Ok(());
            }
            let state = frame.get("data").unwrap_or(&Value::Null);
            let events = if todo_refresh {
                plan(state)
            } else {
                let (current, mode) = goal_in_state(state).map_or((Value::Null, None), |(goal, mode)| (goal, Some(mode)));
                goal(current, mode)
            };
            for event in events {
                self.emit(event).await?;
            }
            return Ok(());
        }
        let Some(index) = self.prompts.iter().position(|prompt| prompt.id == id) else {
            return Ok(());
        };
        let prompt = self.prompts.remove(index);
        if !success {
            let error = frame.get("error").and_then(Value::as_str).unwrap_or("prompt was rejected");
            return Err(HarnessError::Protocol(format!("OMP refused the prompt: {error}")));
        }
        if prompt.refresh_goal && self.goal_request.is_none() {
            self.goal_request = Some(self.refresh_state());
        }
        // Taken without the agent (a local command): the turn is over.
        if frame.pointer("/data/agentInvoked").and_then(Value::as_bool) == Some(false) && self.owed && !self.streaming {
            self.done(DoneStatus::Completed, None).await?;
        }
        Ok(())
    }

    /// The next prompt reaches OMP now: steering the live turn, or starting
    /// the next one when parked. Either way a fresh segment begins.
    async fn steer(&mut self, steer: SteerMessage) -> Result<(), HarnessError> {
        let previous = self.next_assistant();
        self.emit(AgentEvent::Steered {
            assistant_message_id: Some(previous),
            next_assistant_message_id: Some(self.assistant.clone()),
        })
        .await?;
        self.owed = true;
        let images = self.images(&steer.attachments).await;
        self.prompt(&steer.prompt, images);
        Ok(())
    }

    /// Stop the turn in place: OMP aborts it and the session stays up. A stop
    /// with no turn open is spent here, never carried into the next turn.
    fn stop(&mut self) {
        if !self.owed {
            tracing::debug!(target: "zeron_harness::crew_omp", "OMP stop with no turn open");
            return;
        }
        self.stopped = true;
        if self.abort_request.is_none() {
            self.abort_request = Some(self.process.client.send_command("abort", Map::new()));
        }
    }

    /// End the run: abort what runs (OMP may never answer, so it has its
    /// grace and no more), reap the process, then report the turn
    /// interrupted, so no later resume meets the old process.
    async fn interrupt(&mut self) {
        let abort = self.process.client.send_command("abort", Map::new());
        let deadline = Instant::now() + self.options.interrupt_grace;
        loop {
            tokio::select! {
                frame = self.process.frames.recv() => match frame {
                    Some(frame) if kind(&frame) == "response" && str_field(&frame, "id") == abort => break,
                    Some(_) => continue,
                    None => break,
                },
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
        let owed = self.owed;
        self.shutdown().await;
        if owed {
            let _ = self.done(DoneStatus::Interrupted, None).await;
        }
    }

    async fn shutdown(&mut self) {
        self.dialogs.cancel(&self.process.client);
        self.process.child.shutdown(self.options.kill_grace).await;
    }

    /// The run failed: a crash is named with its exit and stderr's last lines
    /// (the pipe's end is usually its first symptom), then reported to the
    /// engine, which ends a turn with it or a parked session quietly.
    async fn fail(&mut self, error: HarnessError) {
        let status = tokio::time::timeout(Duration::from_millis(250), self.process.child.wait())
            .await
            .ok()
            .and_then(Result::ok);
        let error = match status {
            Some(status) => {
                self.process.tail.wait_closed().await;
                HarnessError::Protocol(crate::crash_message("OMP", Some(status), &self.process.tail))
            }
            None => error,
        };
        self.shutdown().await;
        let _ = self.events.send(Err(error)).await;
    }
}

// ---------------------------------------------------------------------------
// Command discovery
// ---------------------------------------------------------------------------

/// A disposable catalog process's commands. OMP pushes a snapshot as it
/// starts and may push richer ones as command providers finish loading: the
/// newest is kept until the stream settles, with `get_available_commands`
/// for an OMP that pushes none. The child is reaped before this returns.
pub(super) async fn command_catalog(mut process: Process, deadline: Duration) -> Result<Vec<SlashCommand>, HarnessError> {
    let result = tokio::time::timeout(deadline, catalog_frames(&mut process))
        .await
        .unwrap_or_else(|_| Err(HarnessError::Protocol("OMP command discovery timed out".into())));
    process.child.shutdown(Duration::ZERO).await;
    result
}

async fn catalog_frames(process: &mut Process) -> Result<Vec<SlashCommand>, HarnessError> {
    let mut latest = None;
    let mut requested = false;
    loop {
        let frame = if latest.is_some() {
            match tokio::time::timeout(CATALOG_SETTLE, process.frames.recv()).await {
                Ok(frame) => frame,
                Err(_) => return Ok(latest.take().expect("a snapshot")),
            }
        } else {
            process.frames.recv().await
        };
        let Some(frame) = frame else {
            let status = process.child.wait().await?;
            if status.success()
                && let Some(commands) = latest
            {
                return Ok(commands);
            }
            process.tail.wait_closed().await;
            return Err(HarnessError::Protocol(crate::crash_message(
                "OMP command discovery",
                Some(status),
                &process.tail,
            )));
        };
        match kind(&frame) {
            "available_commands_update" => {
                if let Some(commands) = commands(&frame) {
                    latest = Some(commands);
                }
            }
            "ready" if !requested => {
                requested = true;
                process.client.send_command("get_available_commands", Map::new());
            }
            "response" if str_field(&frame, "command") == "get_available_commands" => {
                if frame.get("success").and_then(Value::as_bool) != Some(true) {
                    return Err(HarnessError::Protocol(format!(
                        "OMP get_available_commands: {}",
                        frame.get("error").and_then(Value::as_str).unwrap_or("command failed")
                    )));
                }
                latest = Some(frame.get("data").and_then(commands).ok_or_else(|| {
                    HarnessError::Protocol("OMP get_available_commands returned no commands".into())
                })?);
            }
            "agent_start" => {
                return Err(HarnessError::Protocol("OMP command discovery tried to start inference".into()));
            }
            "notice"
                if str_field(&frame, "source") == "command-catalog" && str_field(&frame, "level") == "error" =>
            {
                return Err(HarnessError::Protocol(
                    frame.get("message").and_then(Value::as_str).unwrap_or("OMP command discovery failed").to_owned(),
                ));
            }
            _ => {}
        }
    }
}

/// A short-lived command's whole output, bounded: what `omp models` prints.
pub(super) async fn read_bounded(output: impl AsyncRead + Unpin, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    output.take(limit).read_to_end(&mut bytes).await?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames_of(bytes: &'static [u8], max_frame: usize) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel(16);
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        tokio::spawn(read_frames(bytes, pending, tx, max_frame));
        rx
    }

    /// Crew's `native_chunks_round_trip_and_reject_out_of_order_frames`.
    #[test]
    fn chunks_reassemble_in_order_and_an_out_of_order_run_is_dropped() {
        let payload = json!({"type": "response", "id": "x", "success": true});
        let bytes = serde_json::to_vec(&payload).unwrap();
        let (a, b) = bytes.split_at(bytes.len() / 2);
        let chunk = |index: u64, part: &[u8]| {
            json!({
                "type": "rpc_chunk", "chunkId": "r1", "index": index, "count": 2,
                "byteLength": bytes.len(), "data": base64::engine::general_purpose::STANDARD.encode(part),
            })
        };
        let mut active = None;
        assert!(assemble_chunk(&mut active, &chunk(1, b), MAX_FRAME).is_none());
        assert!(assemble_chunk(&mut active, &chunk(0, a), MAX_FRAME).is_none());
        assert_eq!(assemble_chunk(&mut active, &chunk(1, b), MAX_FRAME), Some(payload));
        // A run declaring more than the bound is refused at its first chunk.
        let mut active = None;
        assert!(assemble_chunk(&mut active, &chunk(0, a), 4).is_none());
        assert!(active.is_none());
    }

    #[tokio::test]
    async fn noise_lone_surrogates_and_oversized_lines_do_not_end_the_stream() {
        let mut frames = frames_of(
            b"not json\n{\"type\":\"message_update\",\"text\":\"cut \\ud83d here\"}\n{\"type\":\"huge\",\"text\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}\r\n{\"type\":\"agent_end\"}\n",
            64,
        );
        assert_eq!(frames.recv().await.unwrap(), json!({"type": "message_update", "text": "cut \u{fffd} here"}));
        assert_eq!(frames.recv().await.unwrap(), json!({"type": "agent_end"}));
        assert!(frames.recv().await.is_none(), "the end of the output closes the channel");
    }

    #[test]
    fn tools_map_to_the_calls_the_transcript_renders() {
        let call = |name: &str, args: Value| {
            match tool_call(&json!({"toolCallId": "t", "toolName": name, "args": args, "intent": "Look around"})) {
                Some(AgentEvent::ToolCall { id, call }) => {
                    assert_eq!(id, "t");
                    call
                }
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(call("bash", json!({"command": "ls"})), ToolCall::Exec { command: "ls".into() });
        assert_eq!(
            call("edit", json!({"path": "a", "old_string": "x", "newText": "y"})),
            ToolCall::EditFile { path: "a".into(), old_string: Some("x".into()), new_string: Some("y".into()) }
        );
        assert_eq!(call("glob", json!({"pattern": "*.rs"})), ToolCall::Glob { pattern: "*.rs".into() });
        assert!(call("task", json!({"prompt": "p"})).is_subagent_spawn());
        // Without the argument a native chip needs, or for any other tool:
        // its intent, with its input.
        assert_eq!(
            call("read", json!({})),
            ToolCall::Unknown { name: "Look around".into(), input: Some(json!({})) }
        );
        let AgentEvent::ToolResult { output, is_error, .. } = tool_result(&json!({
            "toolCallId": "t", "isError": true, "result": {"content": [{"type": "text", "text": "é".repeat(TOOL_OUTPUT_CAP)}]},
        })) else {
            panic!()
        };
        assert!(is_error);
        assert!(output.unwrap().len() <= TOOL_OUTPUT_CAP);
    }

    #[test]
    fn todos_goals_and_commands_read_as_upstream_s_chips() {
        let state = json!({"todoPhases": [{"tasks": [
            {"content": "read", "status": "completed"},
            {"content": "fix", "status": "in_progress"},
            {"content": " ", "status": "pending"},
        ]}]});
        let [AgentEvent::ToolCall { id, call: ToolCall::Todo { items } }, AgentEvent::ToolResult { id: resolved, .. }] = plan(&state) else {
            panic!()
        };
        assert_eq!((id.as_str(), resolved.as_str()), (zeron_proto::LIVE_PLAN_TOOL_ID, zeron_proto::LIVE_PLAN_TOOL_ID));
        assert_eq!(items, [TodoItem::new("read", TodoStatus::Completed), TodoItem::new("fix", TodoStatus::InProgress)]);
        // Each goal change is a chip of its own.
        let ids: Vec<String> = (0..2)
            .map(|_| match goal(json!({"objective": "x"}), None) {
                [AgentEvent::ToolCall { id, .. }, AgentEvent::ToolResult { .. }] => id,
                _ => panic!(),
            })
            .collect();
        assert_ne!(ids[0], ids[1]);
        assert!(is_goal_command(" /goal set x") && is_goal_command("/goal") && !is_goal_command("/goals"));
        let listed = commands(&json!({"commands": [{"name": "advisor", "input": {"hint": "[on|off]"}}]})).unwrap();
        assert_eq!(listed[0].input_hint.as_deref(), Some("[on|off]"));
        assert!(listed.iter().any(|command| command.name == "goal"));
    }

    #[test]
    fn a_message_s_usage_reports_tokens_and_context() {
        let events = usage(&json!({"usage": {"input": 6, "output": 4, "cacheRead": 100, "cacheWrite": 0}}), Some(1000));
        assert_eq!(
            events,
            [
                AgentEvent::Usage { input_tokens: 106, output_tokens: 4 },
                AgentEvent::ContextUsage { tokens: Some(110), window: Some(1000) }
            ]
        );
        assert!(usage(&json!({}), None).is_empty());
        assert_eq!(assistant_error(&json!({"stopReason": "error"})).as_deref(), Some("OMP reported a model error"));
        assert_eq!(assistant_error(&json!({"stopReason": "stop", "errorMessage": "x"})), None);
    }

    /// Choices answer with OMP's own fields, free text with its value, and
    /// a declined or abandoned dialog as cancelled.
    #[tokio::test]
    async fn dialogs_answer_in_omp_s_terms_and_close_when_abandoned() {
        let (host, peer) = tokio::io::duplex(1 << 16);
        let (_peer_output, output) = tokio::io::duplex(64);
        let (client, _frames, _tasks) = transport(host, output);
        let mut sent = BufReader::new(peer).lines();
        let (asked_tx, mut asked) = mpsc::unbounded_channel::<(UserInputQuestion, oneshot::Sender<Vec<UserInputAnswer>>)>();
        let input: Arc<Input> = Arc::new(move |questions: Vec<UserInputQuestion>| {
            let (tx, rx) = oneshot::channel();
            asked_tx.send((questions.into_iter().next().unwrap(), tx)).unwrap();
            rx
        });
        let mut dialogs = Dialogs::default();
        let mut answer = |frame: Value, label: Option<&str>| {
            dialogs.request(&client, &input, &frame);
            let (question, reply) = asked.try_recv().expect("asked");
            let _ = reply.send(label.map(|label| UserInputAnswer { question_id: question.id.clone(), labels: vec![label.into()] }).into_iter().collect());
            question
        };
        let confirm = answer(json!({"id": "c", "method": "confirm", "title": "Provider"}), Some("Continue"));
        assert_eq!((confirm.header.as_str(), confirm.question.as_str()), ("Provider", "Continue?"));
        let select = answer(json!({"id": "s", "method": "select", "message": "Region", "options": ["East", {"label": "West"}]}), Some("West"));
        assert_eq!(select.options, ["East", "West"]);
        let editor = answer(json!({"id": "e", "method": "editor", "prefill": "draft"}), Some("final"));
        assert!(editor.multiline && editor.prefill.as_deref() == Some("draft"));
        answer(json!({"id": "d", "method": "confirm"}), None);
        let mut replies = Vec::new();
        for _ in 0..4 {
            let line = tokio::time::timeout(Duration::from_secs(5), sent.next_line()).await.unwrap().unwrap().unwrap();
            replies.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        replies.sort_by_key(|reply| reply["id"].as_str().unwrap().to_owned());
        assert_eq!(replies[0], json!({"type": "extension_ui_response", "id": "c", "confirmed": true}));
        assert_eq!(replies[1], json!({"type": "extension_ui_response", "id": "d", "cancelled": true}));
        assert_eq!(replies[2], json!({"type": "extension_ui_response", "id": "e", "value": "final"}));
        assert_eq!(replies[3], json!({"type": "extension_ui_response", "id": "s", "value": "West"}));
        // One left open is cancelled when the turn ends.
        dialogs.request(&client, &input, &json!({"id": "open", "method": "input"}));
        let (_question, mut reply) = asked.try_recv().unwrap();
        dialogs.cancel(&client);
        tokio::time::timeout(Duration::from_secs(5), reply.closed()).await.unwrap();
        let line = tokio::time::timeout(Duration::from_secs(5), sent.next_line()).await.unwrap().unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), json!({"type": "extension_ui_response", "id": "open", "cancelled": true}));
    }

    /// Crew's `command_catalog_reports_failures_and_reaps_before_returning`,
    /// and its snapshot rule: a catalog process that fails never hands back
    /// the snapshot it printed first.
    #[cfg(unix)]
    #[tokio::test]
    async fn command_discovery_reports_failures_and_reaps_before_returning() {
        let script = r#"sleep 60 & trap 'kill $!; exit' TERM; printf '%s\n' '{"type":"ready"}'; while IFS= read -r line; do printf '%s\n' "$1"; done"#;
        for (reply, expected) in [
            (
                r#"{"type":"response","command":"get_available_commands","success":false,"error":"catalog provider unavailable"}"#,
                "catalog provider unavailable",
            ),
            (
                r#"{"type":"response","command":"get_available_commands","success":true,"data":{}}"#,
                "returned no commands",
            ),
            (r#"{"type":"agent_start"}"#, "tried to start inference"),
            (
                r#"{"type":"notice","source":"command-catalog","level":"error","message":"catalog inference denied"}"#,
                "catalog inference denied",
            ),
            ("", "timed out"),
        ] {
            let child = catalog_child(&["-c", script, "catalog-test", reply]);
            let pid = child.id().unwrap() as i32;
            let error = command_catalog(Process::new(child, None).unwrap(), Duration::from_secs(1)).await.unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert_group_gone(pid);
        }
        for (exit, ok) in [("0", true), ("17", false)] {
            let script = format!(
                r#"printf '%s\n' '{{"type":"available_commands_update","commands":[{{"name":"advisor"}}]}}'; exit {exit}"#
            );
            let child = catalog_child(&["-c", script.as_str()]);
            let pid = child.id().unwrap() as i32;
            let result = command_catalog(Process::new(child, None).unwrap(), Duration::from_secs(5)).await;
            assert_eq!(result.is_ok(), ok, "exit {exit}: {result:?}");
            if ok {
                assert!(result.unwrap().iter().any(|command| command.name == "advisor"));
            }
            assert_group_gone(pid);
        }
    }

    #[cfg(unix)]
    fn catalog_child(args: &[&str]) -> crate::process::Child {
        let mut command = crate::process::Command::new("/bin/sh");
        command
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        crate::process::owned::configure(&mut command);
        command.spawn().unwrap()
    }

    #[cfg(unix)]
    fn assert_group_gone(pid: i32) {
        // A killed member is gone once init has reaped it, a moment later.
        // Until then macOS answers EPERM for a group of zombies, not ESRCH.
        let gone = || {
            // SAFETY: signal 0 only checks this fixture's group.
            let signalled = unsafe { libc::kill(-pid, 0) };
            signalled == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !gone() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone(), "the catalog's group survived its result");
    }
}
