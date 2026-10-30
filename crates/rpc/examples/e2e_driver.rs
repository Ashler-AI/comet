//! Two-device e2e smoke driver (`scripts/e2e-smoke.sh` runs it).
//!
//! Connects to two running headless engines over their localhost IPC ports and proves
//! the cross-device command plane end to end against a real edge:
//!
//! 1. `LocalDevice` on both — distinct device ids;
//! 2. `Mutate createChat` on A (A hosts the chat, mock harness config);
//! 3. waits until the chat row syncs A → edge → B (`WatchChats` on B);
//! 4. `QueueCommand` a Run on **B** — the doc path: B commits the command into its
//!    replica, nudges A's device room, A's host executor drains and runs it;
//! 5. waits on **B** until the assistant entry lands `complete` with the mock
//!    transcript (A → edge → B), and the session-status row round-trips.
//! 6. stages a deterministic 5,357,034-byte file on B, queues its pending ref,
//!    waits for A's mock completion, then verifies A's bytes and SHA-256 by RPC.
//!
//! Prints `PASS`/`FAIL` lines; exits nonzero on failure.

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};

use zeron_rpc::{RpcClient, connect_ipc, methods};

const STEP_TIMEOUT: Duration = Duration::from_secs(90);
const MOCK_TEXT: &str = "Mock harness reporting in.";

fn fail(message: &str) -> ! {
    eprintln!("FAIL: {message}");
    std::process::exit(1);
}

fn pass(message: &str) {
    println!("PASS: {message}");
}

async fn device_id(client: &RpcClient, label: &str) -> String {
    match client
        .call(methods::LOCAL_DEVICE, serde_json::json!({}))
        .await
    {
        Ok(value) => value
            .get("deviceId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| fail(&format!("{label}: LocalDevice reply missing deviceId"))),
        Err(err) => fail(&format!("{label}: LocalDevice call failed: {err}")),
    }
}

/// Subscribe to a watch-stream and poll items until `predicate` returns `Some`.
///
/// Stream end is NOT a failure: the engine ends watch streams deliberately at
/// lifecycle boundaries (the chat2 cutover drops the s2 handle so watchers
/// converge onto the new room), and the client contract — the UI does exactly
/// this — is to resubscribe. The step deadline still bounds the whole wait.
async fn wait_stream<T>(
    client: &RpcClient,
    method: &str,
    params: serde_json::Value,
    what: &str,
    mut predicate: impl FnMut(&serde_json::Value) -> Option<T>,
) -> T {
    let deadline = Instant::now() + STEP_TIMEOUT;
    'resubscribe: loop {
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            fail(&format!(
                "{what}: timed out after {}s",
                STEP_TIMEOUT.as_secs()
            ));
        }
        let mut rx = match client.subscribe(method, params.clone()).await {
            Ok(rx) => rx,
            Err(err) => fail(&format!("{what}: subscribe {method} failed: {err}")),
        };
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                fail(&format!(
                    "{what}: timed out after {}s",
                    STEP_TIMEOUT.as_secs()
                ));
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(item)) => {
                    if let Some(found) = predicate(&item) {
                        return found;
                    }
                }
                Ok(None) => {
                    // Lifecycle boundary (e.g. chat2 cutover): resubscribe.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue 'resubscribe;
                }
                Err(_) => fail(&format!(
                    "{what}: timed out after {}s",
                    STEP_TIMEOUT.as_secs()
                )),
            }
        }
    }
}

fn apply_transcript_item(
    current: &mut Vec<zeron_doc::SessionMessageEntry>,
    item: &serde_json::Value,
) -> Result<(), String> {
    let frame: zeron_doc::TranscriptFrame =
        serde_json::from_value(item.clone()).map_err(|err| err.to_string())?;
    zeron_doc::apply_transcript_frame(current, frame).map_err(|err| err.to_string())
}

async fn attachment_smoke(
    a: &RpcClient,
    b: &RpcClient,
    chat_id: &str,
    a_dev: &str,
    transcript: &mut Vec<zeron_doc::SessionMessageEntry>,
) {
    let payload: Vec<u8> = (0..5_357_034u32)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect();
    let expected_sha = format!("{:x}", Sha256::digest(&payload));
    let upload_id = uuid::Uuid::new_v4().to_string();
    let file_name = "queued-throughput.bin";
    let pending_ref = format!("pending://{upload_id}/{file_name}");
    let mut staged_chunks = 0;
    for (seq, bytes) in payload.chunks(510_000).enumerate() {
        b.call(
            methods::UPLOAD_CHUNK,
            serde_json::json!({ "uploadId": upload_id, "seq": seq, "data": BASE64.encode(bytes) }),
        )
        .await
        .unwrap_or_else(|err| fail(&format!("stage attachment chunk {seq} on B: {err}")));
        staged_chunks += 1;
    }
    let committed = b.call(
        methods::UPLOAD_COMMIT,
        serde_json::json!({ "uploadId": upload_id, "fileName": file_name }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("commit local attachment on B: {err}")));
    let local_path = committed["path"].as_str()
        .unwrap_or_else(|| fail("B UploadCommit reply missing path"));
    pass(&format!("staged {} bytes on B ({staged_chunks} acknowledged local chunks)", payload.len()));

    // Snapshots may coalesce and acknowledgements may arrive out of order.
    // Log observed bytes without inferring chunk geometry or a throughput bound.
    let mut progress = b.subscribe_scoped(methods::WATCH_TRANSFERS, serde_json::json!({}))
        .await
        .unwrap_or_else(|err| fail(&format!("WatchTransfers on B: {err}")));
    let watched_id = upload_id.clone();
    let progress_task = tokio::spawn(async move {
        while let Some(item) = progress.recv().await {
            let transfers: Vec<zeron_proto::TransferProgress> = serde_json::from_value(item)
                .unwrap_or_else(|err| fail(&format!("decode WatchTransfers: {err}")));
            for transfer in transfers.iter().filter(|t| t.upload_id == watched_id) {
                println!("INFO: remote attachment progress observed: {}/{} bytes acknowledged", transfer.done, transfer.total);
            }
        }
    });
    let previous_assistants: Vec<String> = transcript.iter()
        .filter(|e| e.role == zeron_doc::MessageRole::Assistant)
        .map(|e| e.id.clone())
        .collect();
    let message_id = uuid::Uuid::new_v4().to_string();
    let started = Instant::now();
    b.call(
        methods::QUEUE_COMMAND,
        serde_json::json!({
            "chatId": chat_id,
            "transfers": [{ "uploadId": upload_id, "fileName": file_name }],
            "command": {
                "kind": "run",
                "messageId": message_id,
                "request": {
                    "prompt": format!("e2e smoke: queued file\n\nAttached files (local files):\n- {pending_ref}"),
                    "attachments": [pending_ref],
                    "model": null,
                    "reasoning": null,
                    "cwd": "/tmp",
                    "sandbox": "workspace-write",
                    "autoApprove": true,
                    "resume": null,
                },
            },
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("QueueCommand with attachment on B: {err}")));
    pass("attachment run queued on B with pending ref and transfers (no targetDeviceId)");

    let remote_path = wait_stream(
        b,
        methods::WATCH_DOC_MESSAGES,
        serde_json::json!({ "chatId": chat_id }),
        "attachment run completed by A and synced to B",
        |item| {
            apply_transcript_item(transcript, item)
                .unwrap_or_else(|err| fail(&format!("attachment transcript: {err}")));
            let mut completed = transcript.iter().filter(|entry| {
                entry.role == zeron_doc::MessageRole::Assistant
                    && entry.status == Some(zeron_doc::MessageStatus::Complete)
                    && !previous_assistants.contains(&entry.id)
            });
            let assistant = completed.next()?;
            if completed.next().is_some() {
                fail("attachment run produced multiple completed assistant entries");
            }
            if assistant.device_id != a_dev {
                fail("attachment run completed on a device other than host A");
            }
            let text = serde_json::to_string(assistant)
                .unwrap_or_else(|err| fail(&format!("serialize attachment assistant: {err}")));
            if !text.contains(MOCK_TEXT) {
                fail("attachment run lacks the mock transcript");
            }
            let user = transcript.iter().find(|entry| entry.id == message_id
                && entry.role == zeron_doc::MessageRole::User)?;
            let path = user.parts.iter().find_map(|part| match part {
                zeron_doc::MessagePart::Text { text, .. } => text.lines()
                    .find_map(|line| line.strip_prefix("- ").filter(|path| path.ends_with(file_name)))
                    .map(str::to_owned),
                _ => None,
            })?;
            if path.starts_with("pending://") || path == local_path {
                fail("attachment user entry did not resolve to host A's local file");
            }
            Some(path)
        },
    ).await;
    progress_task.abort();
    let _ = progress_task.await;
    pass(&format!("attachment mock run completed on A and synced to B in {:.3}s since queueing", started.elapsed().as_secs_f64()));

    let mut received = Vec::with_capacity(payload.len());
    let mut read_chunks = 0;
    tokio::time::timeout(STEP_TIMEOUT, async {
        loop {
            let chunk = a.call(
                methods::READ_ATTACHMENT_CHUNK,
                serde_json::json!({ "path": remote_path, "offset": received.len() }),
            ).await.unwrap_or_else(|err| fail(&format!("read committed attachment on A: {err}")));
            let data = chunk["data"].as_str()
                .unwrap_or_else(|| fail("ReadAttachmentChunk missing data"));
            received.extend(BASE64.decode(data)
                .unwrap_or_else(|err| fail(&format!("decode A attachment chunk: {err}"))));
            read_chunks += 1;
            if chunk["nextOffset"].as_u64() != Some(received.len() as u64)
                || received.len() > payload.len()
            {
                fail("A attachment readback returned an invalid offset or excess bytes");
            }
            match chunk["done"].as_bool() {
                Some(true) => break,
                Some(false) if !data.is_empty() => {},
                _ => fail("A attachment readback made no progress or omitted done"),
            }
        }
    }).await.unwrap_or_else(|_| fail("A attachment readback timed out"));
    if received != payload {
        fail("A committed attachment differs byte-for-byte from the synthetic payload");
    }
    let received_sha = format!("{:x}", Sha256::digest(&received));
    if received_sha != expected_sha {
        fail("A committed attachment SHA-256 mismatch");
    }
    pass(&format!("A attachment byte-exact: {} bytes, SHA-256 {received_sha} ({read_chunks} RPC readback chunks)", received.len()));
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let a_port: u16 = args
        .next()
        .unwrap_or_else(|| "27801".into())
        .parse()
        .expect("A port");
    let b_port: u16 = args
        .next()
        .unwrap_or_else(|| "27802".into())
        .parse()
        .expect("B port");

    // Each engine publishes its IPC bearer at `{data_dir}/ipc-token`.
    let a_dir = args.next().map(std::path::PathBuf::from);
    let b_dir = args.next().map(std::path::PathBuf::from);

    let a = connect_ipc(a_port, a_dir.as_deref())
        .await
        .unwrap_or_else(|err| fail(&format!("connect device A ipc :{a_port}: {err}")));
    let b = connect_ipc(b_port, b_dir.as_deref())
        .await
        .unwrap_or_else(|err| fail(&format!("connect device B ipc :{b_port}: {err}")));

    // 1. Distinct identities.
    let a_dev = device_id(&a, "A").await;
    let b_dev = device_id(&b, "B").await;
    if a_dev == b_dev {
        fail("A and B report the same deviceId — data dirs are shared");
    }
    pass(&format!("devices distinct (A={a_dev} B={b_dev})"));

    // 2. Create a space + chat on A, hosted by A, with the mock harness (the
    //    space fixes device + cwd — the chat row derives both from it).
    let space_id = uuid::Uuid::new_v4().to_string();
    a.call(
        methods::MUTATE,
        serde_json::json!({
            "op": "createSpace",
            "spaceId": space_id,
            "deviceId": a_dev,
            "path": "/tmp",
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("createSpace on A: {err}")));
    let chat_id = uuid::Uuid::new_v4().to_string();
    a.call(
        methods::MUTATE,
        serde_json::json!({
            "op": "createChat",
            "chatId": chat_id,
            "spaceId": space_id,
            "config": {
                "harness": "mock",
                "model": null,
                "reasoning": null,
                "sandbox": "workspace-write",
            },
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("createChat on A: {err}")));
    pass(&format!("space + chat created on A ({chat_id})"));

    // 2b. Space row syncs A → edge → B (WatchSpaces).
    let space_device = wait_stream(
        &b,
        methods::WATCH_SPACES,
        serde_json::json!({}),
        "space row visible on B",
        |item| {
            item.as_array()?
                .iter()
                .find(|space| space.get("id").and_then(|v| v.as_str()) == Some(space_id.as_str()))
                .and_then(|space| space.get("deviceId")?.as_str().map(str::to_string))
        },
    )
    .await;
    if space_device != a_dev {
        fail(&format!(
            "space synced to B but owned by {space_device}, expected {a_dev}"
        ));
    }
    pass("space row synced A -> edge -> B (owner = A)");

    // 3. Workspace sync A → edge → B: the chat row appears in B's WatchChats.
    let hosted_by = wait_stream(
        &b,
        methods::WATCH_CHATS,
        serde_json::json!({}),
        "chat row visible on B",
        |item| {
            item.as_array()?
                .iter()
                .find(|chat| chat.get("id").and_then(|v| v.as_str()) == Some(chat_id.as_str()))
                .and_then(|chat| chat.get("deviceId")?.as_str().map(str::to_string))
        },
    )
    .await;
    if hosted_by != a_dev {
        fail(&format!(
            "chat synced to B but hosted by {hosted_by}, expected {a_dev}"
        ));
    }
    pass("chat row synced A -> edge -> B (host = A)");

    // 4. Queue the run from B THROUGH THE DOC (no targetDeviceId): B commits the
    //    command into its replica; the nudge + room sync hand it to A's executor.
    let message_id = uuid::Uuid::new_v4().to_string();
    b.call(
        methods::QUEUE_COMMAND,
        serde_json::json!({
            "chatId": chat_id,
            "command": {
                "kind": "run",
                "messageId": message_id,
                "request": {
                    "prompt": "e2e smoke: report in",
                    "model": null,
                    "reasoning": null,
                    "cwd": "/tmp",
                    "sandbox": "workspace-write",
                    "autoApprove": true,
                    "resume": null,
                },
            },
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("QueueCommand on B: {err}")));
    pass("run command queued on B via the doc command queue");

    // 5a. Assistant entry executed by A arrives back on B, complete, with the
    // mock text. WatchDocMessages speaks the framed protocol: a full `reset`
    // list on attach, then `upsert`/`append` deltas. Materialize every frame
    // through the shared parser so candidates persist across incremental frames.
    let mut transcript = Vec::new();
    let (by_device, text) = wait_stream(
        &b,
        methods::WATCH_DOC_MESSAGES,
        serde_json::json!({ "chatId": chat_id }),
        "assistant transcript on B",
        |item| {
            apply_transcript_item(&mut transcript, item)
                .unwrap_or_else(|err| fail(&format!("assistant transcript on B: {err}")));
            let entry = transcript.iter().find(|entry| {
                entry.role == zeron_doc::MessageRole::Assistant
                    && entry.status == Some(zeron_doc::MessageStatus::Complete)
            })?;
            let device = entry.device_id.clone();
            let text = serde_json::to_string(entry)
                .unwrap_or_else(|err| fail(&format!("serialize assistant transcript: {err}")));
            Some((device, text))
        },
    )
    .await;
    if by_device != a_dev {
        fail(&format!(
            "assistant entry written by {by_device}, expected host {a_dev}"
        ));
    }
    if !text.contains(MOCK_TEXT) {
        fail(&format!(
            "assistant entry lacks the mock transcript: {text}"
        ));
    }
    pass("assistant entry (host-executed, status=complete) synced back to B");

    // 5b. Session-status row round-trips: B sees A's session for this chat go idle.
    wait_stream(
        &b,
        methods::WATCH_SESSIONS,
        serde_json::json!({}),
        "session status on B",
        |item| {
            item.as_array()?.iter().find(|session| {
                session.get("chatId").and_then(|v| v.as_str()) == Some(chat_id.as_str())
                    && session.get("deviceId").and_then(|v| v.as_str()) == Some(a_dev.as_str())
                    && session.get("status").and_then(|v| v.as_str()) == Some("idle")
            })?;
            Some(())
        },
    )
    .await;
    pass("session status (A working -> idle) round-tripped to B");

    attachment_smoke(&a, &b, &chat_id, &a_dev, &mut transcript).await;

    println!("PASS: two-device e2e smoke complete");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_reset_frame_is_materialized() {
        let mut transcript = Vec::new();
        apply_transcript_item(
            &mut transcript,
            &serde_json::json!({
                "reset": [{
                    "id": "assistant-1",
                    "role": "assistant",
                    "parts": [{ "kind": "text", "id": "t0", "text": MOCK_TEXT }],
                    "createdAt": 1,
                    "deviceId": "device-a",
                    "status": "complete"
                }]
            }),
        )
        .expect("reset frame");

        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].device_id, "device-a");
        assert_eq!(
            transcript[0].status,
            Some(zeron_doc::MessageStatus::Complete)
        );
    }
}
