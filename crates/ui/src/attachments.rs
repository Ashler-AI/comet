//! Attachments (feature-inventory §1.7/§1.8): composer-staged images and
//! pasted text, chunked upload to the chat's host device, the plain-text
//! attachment-ref transport that rides the prompt, the transcript read-back
//! cache for images, and the full-size image preview lightbox.
//!
//! Ports of comet's `composer/use-attachments.ts` (staging/upload),
//! `control/message-attachments.ts` (the `withAttachments` /
//! `parseUserMessageImages` text transport — attachment refs are embedded in
//! the user message's plain text, which is exactly what persists in the doc),
//! and `lib/transcript-attachment-cache.ts` (decoded-image cache keyed by
//! `(deviceId, path)`, seeded locally after a send so own bubbles never
//! round-trip).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use gpui::{
    AnyElement, BackgroundExecutor, Image, ImageFormat, ObjectFit, SharedString, Size,
    StyledImage as _, div, img, prelude::*, px,
};

use crate::state::EngineHandle;
use crate::theme::ink;
use comet_rpc::methods;

/// use-attachments.ts `MAX_ATTACHMENT_BYTES`.
pub const MAX_ATTACHMENT_BYTES: u64 = 24 * 1024 * 1024;
/// Folder selections are expanded in-memory before upload. Keep that expansion
/// bounded so choosing a repository cannot accidentally consume unbounded RAM.
pub const MAX_FOLDER_ATTACHMENT_FILES: usize = 200;
pub const MAX_FOLDER_ATTACHMENT_BYTES: u64 = 128 * 1024 * 1024;
const FOLDER_NAME_PREFIX: &str = "cf1-";
const MAX_FOLDER_UPLOAD_NAME_BYTES: usize = 240;
/// Base64 chars per `UploadChunk` (comet state.ts `UPLOAD_CHUNK` — sized for
/// the relay when the target device is remote).
pub const UPLOAD_CHUNK_B64_CHARS: usize = 60_000;
/// state.ts `MAX_ATTACHMENT_READ_CHUNKS` — bounds the read-back loop.
const MAX_READ_CHUNKS: usize = 1_000;

// ---------------------------------------------------------------------------
// Text transport (message-attachments.ts)
// ---------------------------------------------------------------------------

/// The body used for image-only sends (`use-attachments.ts`).
pub const ATTACHMENT_ONLY_TEXT: &str = "See the attached image(s).";
/// The body used when one or more non-image files are the only prompt content.
pub const FILE_ATTACHMENT_ONLY_TEXT: &str = "See the attached file(s).";
/// Historical placeholder persisted by large-paste-only messages.
const LEGACY_TEXT_ATTACHMENT_ONLY_TEXT: &str = "See the attached text.";

/// How attachments ride the prompt: plain local paths appended to the text.
/// Files are staged on the device that runs the agent, so the agent can open
/// them with its own tools; the same text persists as the user doc entry.
pub fn with_attachment_files(text: &str, image_paths: &[String], file_paths: &[String]) -> String {
    if image_paths.is_empty() && file_paths.is_empty() {
        return text.to_string();
    }
    let body = if !text.is_empty() {
        text
    } else if file_paths.is_empty() {
        ATTACHMENT_ONLY_TEXT
    } else {
        FILE_ATTACHMENT_ONLY_TEXT
    };
    let mut content = String::with_capacity(
        body.len()
            + image_paths.iter().map(String::len).sum::<usize>()
            + file_paths.iter().map(String::len).sum::<usize>()
            + 128,
    );
    content.push_str(body);
    if !image_paths.is_empty() {
        content.push_str("\n\nAttached images (local files — open them to view):");
        for path in image_paths {
            let _ = write!(content, "\n- {path}");
        }
    }
    if !file_paths.is_empty() {
        content.push_str("\n\nAttached files (local files — open them to inspect):");
        for path in file_paths {
            let _ = write!(content, "\n- {path}");
        }
    }
    content
}

/// Compatibility entry point for the existing image-only transport.
pub fn with_attachments(text: &str, paths: &[String]) -> String {
    with_attachment_files(text, paths, &[])
}

/// An attachment ref parsed back out of a user message's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserImageAttachment {
    pub id: String,
    pub path: String,
    pub name: String,
}

/// A non-image file ref parsed back out of a user message's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserFileAttachment {
    pub id: String,
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUserMessage {
    /// The visible prompt (attachment-ref trailer already stripped).
    pub text: String,
    pub attachments: Vec<UserImageAttachment>,
    pub file_attachments: Vec<UserFileAttachment>,
}

fn folder_upload_name(display_name: &str) -> Option<String> {
    let mut encoded = String::with_capacity(display_name.len() + FOLDER_NAME_PREFIX.len());
    encoded.push_str(FOLDER_NAME_PREFIX);
    for byte in display_name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "_{byte:02X}");
        }
        if encoded.len() > MAX_FOLDER_UPLOAD_NAME_BYTES {
            return None;
        }
    }
    Some(encoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_folder_upload_name(name: &str) -> Option<String> {
    let encoded = name.strip_prefix(FOLDER_NAME_PREFIX)?.as_bytes();
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        if encoded[index] == b'_' {
            let high = hex_value(*encoded.get(index + 1)?)?;
            let low = hex_value(*encoded.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(encoded[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn name_from_path(path: &str) -> String {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .map(str::trim)
        .unwrap_or_default();
    let name = name
        .split_once('-')
        .filter(|(prefix, _)| {
            prefix.len() == 8 && prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map_or(name, |(_, original)| original);
    if name.is_empty() {
        "attachment".to_string()
    } else {
        decode_folder_upload_name(name).unwrap_or_else(|| name.to_string())
    }
}

/// Find a refs trailer headed by `needle` (case-insensitive). Returns the byte
/// offsets for the preceding body gap and the first refs line.
fn find_refs_marker(content: &str, needle: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    let needle = format!("\n\n{needle}");
    let gap = lower.find(&needle)?;
    let line_start = gap + 2;
    let line_end = content[line_start..]
        .find('\n')
        .map(|p| line_start + p)
        .unwrap_or(content.len());
    let line = content[line_start..line_end].trim_end_matches('\r');
    line.ends_with("):")
        .then_some((gap, (line_end + 1).min(content.len())))
}

fn ref_paths(content: &str, start: usize, end: usize) -> Vec<String> {
    content[start..end]
        .lines()
        .filter_map(|line| {
            let path = line.trim_start().strip_prefix("- ")?.trim();
            (!path.is_empty()).then(|| path.to_string())
        })
        .collect()
}

fn ref_paths_for_marker(
    content: &str,
    marker: Option<(usize, usize)>,
    markers: &[Option<(usize, usize)>],
) -> Vec<String> {
    let Some((gap, start)) = marker else {
        return Vec::new();
    };
    let end = markers
        .iter()
        .flatten()
        .map(|(next_gap, _)| *next_gap)
        .filter(|next_gap| *next_gap > gap)
        .min()
        .unwrap_or(content.len());
    ref_paths(content, start, end)
}

/// Split the visible prompt from image and file attachment trailers.
pub fn parse_user_message_attachments(content: &str) -> ParsedUserMessage {
    let image_marker = find_refs_marker(content, "attached images (local files");
    let file_marker = find_refs_marker(content, "attached files (local files");
    // Existing transcripts used a separate marker for large pasted text.
    let legacy_text_marker = find_refs_marker(content, "attached text (local files");
    let markers = [image_marker, file_marker, legacy_text_marker];
    let image_paths = ref_paths_for_marker(content, image_marker, &markers);
    let mut file_paths = ref_paths_for_marker(content, file_marker, &markers);
    let legacy_text_paths = ref_paths_for_marker(content, legacy_text_marker, &markers);
    if file_marker.map(|(gap, _)| gap) <= legacy_text_marker.map(|(gap, _)| gap) {
        file_paths.extend(legacy_text_paths);
    } else {
        let mut ordered = legacy_text_paths;
        ordered.extend(file_paths);
        file_paths = ordered;
    }
    if image_paths.is_empty() && file_paths.is_empty() {
        return ParsedUserMessage {
            text: content.to_string(),
            attachments: Vec::new(),
            file_attachments: Vec::new(),
        };
    }
    let mut body_end = content.len();
    if !image_paths.is_empty() {
        body_end = body_end.min(image_marker.expect("non-empty image refs require marker").0);
    }
    if !file_paths.is_empty() {
        if let Some((gap, _)) = file_marker {
            body_end = body_end.min(gap);
        }
        if let Some((gap, _)) = legacy_text_marker {
            body_end = body_end.min(gap);
        }
    }
    let body = content[..body_end].trim_end();
    let attachments = image_paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| UserImageAttachment {
            id: format!("{index}:{path}"),
            name: name_from_path(&path),
            path,
        })
        .collect();
    let file_attachments = file_paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| UserFileAttachment {
            id: format!("{index}:{path}"),
            name: name_from_path(&path),
            path,
        })
        .collect();
    ParsedUserMessage {
        text: if matches!(
            body.trim(),
            ATTACHMENT_ONLY_TEXT | FILE_ATTACHMENT_ONLY_TEXT | LEGACY_TEXT_ATTACHMENT_ONLY_TEXT
        ) {
            String::new()
        } else {
            body.to_string()
        },
        attachments,
        file_attachments,
    }
}

/// What the rail/sidebar shows for a user message with no visible prompt.
pub fn user_message_rail_text(content: &str) -> String {
    let parsed = parse_user_message_attachments(content);
    if !parsed.text.trim().is_empty() {
        return parsed.text;
    }
    match (parsed.attachments.len(), parsed.file_attachments.len()) {
        (0, 0) => content.to_string(),
        (1, 0) => "Attached image".to_string(),
        (images, 0) => format!("{images} attached images"),
        (0, 1) => "Attached file".to_string(),
        (0, files) => format!("{files} attached files"),
        (images, files) => format!("{} attached files", images + files),
    }
}

// ---------------------------------------------------------------------------
// Staging (use-attachments.ts intake)
// ---------------------------------------------------------------------------

/// Content staged in the composer before upload. Bytes remain shared through
/// the thumbnail/preview/upload path instead of being copied between layers.
#[derive(Clone)]
pub enum StagedAttachmentContent {
    Image(Arc<Image>),
    Text(Arc<str>),
    File(Arc<[u8]>),
}

#[derive(Clone)]
pub struct StagedAttachment {
    pub id: String,
    /// Filename committed on the agent host. Folder-relative components are
    /// encoded into this basename so recursive selections remain distinguishable.
    pub name: String,
    /// User-facing source label, including the selected folder name.
    pub display_name: String,
    pub content: StagedAttachmentContent,
}

pub fn display_basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

impl StagedAttachment {
    pub fn bytes(&self) -> &[u8] {
        match &self.content {
            StagedAttachmentContent::Image(image) => &image.bytes,
            StagedAttachmentContent::Text(text) => text.as_bytes(),
            StagedAttachmentContent::File(bytes) => bytes,
        }
    }

    pub fn image(&self) -> Option<&Arc<Image>> {
        match &self.content {
            StagedAttachmentContent::Image(image) => Some(image),
            StagedAttachmentContent::Text(_) | StagedAttachmentContent::File(_) => None,
        }
    }

    pub fn display_basename(&self) -> &str {
        display_basename(&self.display_name)
    }
}

/// Image formats the whole pipeline supports: intersection of gpui's decoders
/// and the engine's `mime_by_ext` read-back jail.
pub fn format_by_extension(path: &Path) -> Option<ImageFormat> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::Webp),
        "svg" => Some(ImageFormat::Svg),
        "bmp" => Some(ImageFormat::Bmp),
        "tif" | "tiff" => Some(ImageFormat::Tiff),
        _ => None,
    }
}

/// use-attachments.ts `ensureExtension`: pasted screenshots often arrive as a
/// bare "image" — make sure the staged name carries a type-matching extension.
pub fn ensure_extension(name: &str, format: ImageFormat) -> String {
    let has_ext = name
        .rsplit_once('.')
        .map(|(stem, ext)| {
            !stem.is_empty()
                && (2..=5).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or(false);
    if has_ext {
        name.to_string()
    } else {
        format!("{name}.{}", format.extension())
    }
}

/// Stage a file from disk (picker / drop / pasted path). Images retain decoded
/// thumbnail content; every other regular file retains its raw bytes for upload.
/// `Err` carries the user-facing failure message.
pub fn stage_file(path: &Path) -> Result<StagedAttachment, String> {
    let source_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "attachment".to_string());
    let meta = std::fs::metadata(path).map_err(|_| format!("{source_name} could not be read."))?;
    if !meta.is_file() {
        return Err(format!("{source_name} is not a file."));
    }
    if meta.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!("{source_name} is too large (24 MB max)."));
    }
    let bytes = std::fs::read(path).map_err(|_| format!("{source_name} could not be read."))?;
    let (name, content) = match format_by_extension(path) {
        Some(format) => (
            ensure_extension(&source_name, format),
            StagedAttachmentContent::Image(Arc::new(Image::from_bytes(format, bytes))),
        ),
        None => (
            source_name,
            StagedAttachmentContent::File(Arc::<[u8]>::from(bytes)),
        ),
    };
    Ok(StagedAttachment {
        id: uuid::Uuid::new_v4().to_string(),
        display_name: name.clone(),
        name,
        content,
    })
}

/// Expand one picker/drop selection. Directories become one staged attachment
/// per regular descendant, sorted by relative path. Symlinks are rejected
/// instead of followed so a folder cannot escape its selected tree or cycle.
pub fn stage_selected_path(path: &Path) -> Result<Vec<StagedAttachment>, String> {
    let source_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "folder".to_string());
    let metadata =
        std::fs::metadata(path).map_err(|_| format!("{source_name} could not be read."))?;
    if !metadata.is_dir() {
        return stage_file(path).map(|attachment| vec![attachment]);
    }

    let mut directories = vec![path.to_path_buf()];
    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    while let Some(directory) = directories.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|_| format!("{source_name} could not be read."))?;
        for entry in entries {
            let entry = entry.map_err(|_| format!("{source_name} could not be read."))?;
            let entry_path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|_| format!("{} could not be read.", entry_path.display()))?;
            if file_type.is_symlink() {
                let relative = entry_path.strip_prefix(path).unwrap_or(&entry_path);
                return Err(format!(
                    "{} is a symbolic link; folder attachments don't follow links.",
                    relative.display()
                ));
            }
            if file_type.is_dir() {
                directories.push(entry_path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if files.len() == MAX_FOLDER_ATTACHMENT_FILES {
                return Err(format!(
                    "{source_name} contains more than {MAX_FOLDER_ATTACHMENT_FILES} files."
                ));
            }
            let file_bytes = entry
                .metadata()
                .map_err(|_| format!("{} could not be read.", entry_path.display()))?
                .len();
            total_bytes = total_bytes
                .checked_add(file_bytes)
                .ok_or_else(|| format!("{source_name} is too large."))?;
            if total_bytes > MAX_FOLDER_ATTACHMENT_BYTES {
                return Err(format!("{source_name} is too large (128 MB total max)."));
            }
            files.push(entry_path);
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(format!("{source_name} contains no regular files."));
    }

    let mut attachments = Vec::with_capacity(files.len());
    for file in files {
        let relative = file.strip_prefix(path).unwrap_or(&file);
        let display_path = Path::new(&source_name).join(relative);
        let display_name = display_path.to_string_lossy().into_owned();
        let upload_name = folder_upload_name(&display_name)
            .ok_or_else(|| format!("{display_name} has a path too long to attach."))?;
        let mut attachment = stage_file(&file)?;
        attachment.name = upload_name;
        attachment.display_name = display_name;
        attachments.push(attachment);
    }
    Ok(attachments)
}

/// Stage an image pasted from the clipboard.
pub fn stage_clipboard_image(image: Image) -> StagedAttachment {
    let format = image.format;
    let name = ensure_extension("image", format);
    StagedAttachment {
        id: uuid::Uuid::new_v4().to_string(),
        display_name: name.clone(),
        name,
        content: StagedAttachmentContent::Image(Arc::new(image)),
    }
}

/// Stage a large clipboard paste as a UTF-8 text file without touching disk.
pub fn stage_pasted_text(text: String) -> StagedAttachment {
    StagedAttachment {
        id: uuid::Uuid::new_v4().to_string(),
        name: "pasted-text.txt".to_string(),
        display_name: "pasted-text.txt".to_string(),
        content: StagedAttachmentContent::Text(Arc::from(text)),
    }
}

// ---------------------------------------------------------------------------
// Upload (state.ts uploadAttachment) + read-back (state.ts readAttachmentImage)
// ---------------------------------------------------------------------------

pub(crate) fn with_target(
    mut params: serde_json::Value,
    target_device_id: Option<&str>,
) -> serde_json::Value {
    if let (Some(target), Some(map)) = (target_device_id, params.as_object_mut()) {
        map.insert("targetDeviceId".into(), target.into());
    }
    params
}

/// Per-call deadlines (desktop state.ts): a stalled-but-open relay link never
/// fails an RPC on its own, so every attachment call races a timer. The first
/// chunk gets 90s (a cold dial to a remote device), later chunks 30s; commit
/// 150s (it must outlast the engine's cross-device assemble); reads 20s.
const FIRST_CHUNK_TIMEOUT: Duration = Duration::from_secs(90);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(150);
const READ_CHUNK_TIMEOUT: Duration = Duration::from_secs(20);

/// Race an RPC against `timeout` on the gpui background executor (these
/// futures run under `cx.spawn`, so tokio's timer reactor isn't available).
pub(crate) async fn call_with_timeout(
    engine: &EngineHandle,
    executor: &BackgroundExecutor,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let call = engine.client().call(method, params);
    let timer = executor.timer(timeout);
    futures::pin_mut!(call);
    match futures::future::select(call, timer).await {
        futures::future::Either::Left((result, _)) => result.map_err(|e| e.to_string()),
        futures::future::Either::Right(_) => Err(format!("{method} timed out")),
    }
}

/// Chunked upload: base64 the bytes, `UploadChunk{uploadId,seq,data}` per 60KB
/// slice (positional `seq` makes the cheap retry idempotent), then
/// `UploadCommit{uploadId,fileName}` → the durable absolute path on the target
/// device. Errors return the raw cause (the composer shows friendly copy).
pub async fn upload_attachment(
    engine: &EngineHandle,
    executor: &BackgroundExecutor,
    target_device_id: Option<&str>,
    attachment: &StagedAttachment,
) -> Result<String, String> {
    let b64 = BASE64.encode(attachment.bytes());
    let upload_id = uuid::Uuid::new_v4().to_string();
    let mut start = 0usize;
    let mut seq = 0u64;
    loop {
        let end = (start + UPLOAD_CHUNK_B64_CHARS).min(b64.len());
        let params = with_target(
            serde_json::json!({ "uploadId": upload_id, "seq": seq, "data": &b64[start..end] }),
            target_device_id,
        );
        let timeout = if seq == 0 {
            FIRST_CHUNK_TIMEOUT
        } else {
            CHUNK_TIMEOUT
        };
        // One transient blip must not abort a ~400-chunk upload; `seq` slots
        // are idempotent engine-side, so a blind re-send is safe (timeouts
        // retry too, like the original's per-chunk `withTimeout` + retry ×2).
        let mut attempt = 0u32;
        loop {
            match call_with_timeout(
                engine,
                executor,
                methods::UPLOAD_CHUNK,
                params.clone(),
                timeout,
            )
            .await
            {
                Ok(_) => break,
                Err(err) if attempt < 2 => {
                    attempt += 1;
                    tracing::debug!(error = %err, seq, "upload chunk retry");
                }
                Err(err) => return Err(err),
            }
        }
        start = end;
        seq += 1;
        if start >= b64.len() {
            break;
        }
    }
    let params = with_target(
        serde_json::json!({ "uploadId": upload_id, "fileName": attachment.name }),
        target_device_id,
    );
    let reply = call_with_timeout(
        engine,
        executor,
        methods::UPLOAD_COMMIT,
        params,
        COMMIT_TIMEOUT,
    )
    .await?;
    reply
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "upload commit returned no path".to_string())
}

/// A transcript image read back from the owning device.
pub struct LoadedAttachmentImage {
    pub name: String,
    pub image: Arc<Image>,
}

/// `ReadAttachmentChunk` loop: 45KB base64 chunks until `done` (bounded, with
/// the same stuck-offset guard as comet's `readAttachmentImage`).
pub async fn read_attachment_image(
    engine: &EngineHandle,
    executor: &BackgroundExecutor,
    target_device_id: Option<&str>,
    path: &str,
) -> Option<LoadedAttachmentImage> {
    let mut name = String::new();
    let mut mime = String::new();
    let mut b64 = String::new();
    let mut offset = 0u64;
    let mut done = false;
    for _ in 0..MAX_READ_CHUNKS {
        let params = with_target(
            serde_json::json!({ "path": path, "offset": offset }),
            target_device_id,
        );
        let chunk = call_with_timeout(
            engine,
            executor,
            methods::READ_ATTACHMENT_CHUNK,
            params,
            READ_CHUNK_TIMEOUT,
        )
        .await
        .ok()?;
        name = chunk.get("name")?.as_str()?.to_string();
        mime = chunk.get("mimeType")?.as_str()?.to_string();
        b64.push_str(chunk.get("data")?.as_str()?);
        done = chunk.get("done")?.as_bool()?;
        if done {
            break;
        }
        let next = chunk.get("nextOffset")?.as_u64()?;
        if next <= offset {
            return None;
        }
        offset = next;
    }
    if !done || b64.is_empty() {
        return None;
    }
    let bytes = BASE64.decode(b64.as_bytes()).ok()?;
    let format = ImageFormat::from_mime_type(&mime).unwrap_or(ImageFormat::Png);
    Some(LoadedAttachmentImage {
        name: if name.is_empty() {
            name_from_path(path)
        } else {
            name
        },
        image: Arc::new(Image::from_bytes(format, bytes)),
    })
}

// ---------------------------------------------------------------------------
// Transcript image cache (transcript-attachment-cache.ts)
// ---------------------------------------------------------------------------

/// A decoded transcript image, ready for `img(...)`.
#[derive(Clone)]
pub struct CachedAttachmentImage {
    pub name: SharedString,
    pub image: Arc<Image>,
}

/// What a render pass sees for one `(deviceId, path)` source.
#[derive(Clone)]
pub enum AttachmentSnapshot {
    Loading,
    Loaded(CachedAttachmentImage),
    /// Load failed; `retry_in` is how long until [`begin_load`] would hand out
    /// another attempt (the exponential 2s→15s ladder from user-attachments.tsx).
    Error {
        retry_in: Duration,
    },
}

enum CacheEntry {
    Loading {
        attempts: u32,
    },
    Loaded {
        image: CachedAttachmentImage,
        bytes: usize,
        last_used: u64,
    },
    Error {
        attempts: u32,
        at: Instant,
    },
}

fn retry_delay(attempts: u32) -> Duration {
    Duration::from_millis((2_000u64 << attempts.min(3)).min(15_000))
}

/// Byte budget for retained encoded images. The decoded copies gpui holds are
/// proportional (and usually larger), so bounding the encoded side bounds both
/// — this cache previously grew for the process lifetime with no eviction.
const IMAGE_CACHE_BUDGET_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct ImageCache {
    map: HashMap<(String, String), CacheEntry>,
    /// Monotonic access clock for LRU ordering.
    tick: u64,
    loaded_bytes: usize,
    /// Evicted images awaiting `flush_evicted` (freeing needs `&mut App`,
    /// which eviction sites — async load completions — don't always have).
    pending_free: Vec<Arc<Image>>,
}

impl ImageCache {
    fn insert_loaded(&mut self, key: (String, String), image: CachedAttachmentImage) {
        let bytes = image.image.bytes.len();
        self.tick += 1;
        if let Some(CacheEntry::Loaded { image, bytes, .. }) = self.map.insert(
            key.clone(),
            CacheEntry::Loaded {
                image,
                bytes,
                last_used: self.tick,
            },
        ) {
            self.loaded_bytes = self.loaded_bytes.saturating_sub(bytes);
            self.pending_free.push(image.image);
        }
        self.loaded_bytes += bytes;
        while self.loaded_bytes > IMAGE_CACHE_BUDGET_BYTES {
            let oldest = self
                .map
                .iter()
                .filter(|(k, _)| **k != key)
                .filter_map(|(k, e)| match e {
                    CacheEntry::Loaded { last_used, .. } => Some((*last_used, k.clone())),
                    _ => None,
                })
                .min();
            let Some((_, evict_key)) = oldest else { break };
            if let Some(CacheEntry::Loaded { image, bytes, .. }) = self.map.remove(&evict_key) {
                self.loaded_bytes = self.loaded_bytes.saturating_sub(bytes);
                self.pending_free.push(image.image);
            }
        }
    }
}

fn cache() -> &'static Mutex<ImageCache> {
    static CACHE: OnceLock<Mutex<ImageCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ImageCache::default()))
}

fn key(device_id: &str, path: &str) -> (String, String) {
    (device_id.to_string(), path.to_string())
}

pub fn attachment_snapshot(device_id: &str, path: &str) -> AttachmentSnapshot {
    let mut cache = cache().lock().unwrap();
    let tick = {
        cache.tick += 1;
        cache.tick
    };
    match cache.map.get_mut(&key(device_id, path)) {
        Some(CacheEntry::Loaded {
            image, last_used, ..
        }) => {
            *last_used = tick;
            AttachmentSnapshot::Loaded(image.clone())
        }
        Some(CacheEntry::Error { attempts, at }) => AttachmentSnapshot::Error {
            retry_in: retry_delay(attempts.saturating_sub(1)).saturating_sub(at.elapsed()),
        },
        _ => AttachmentSnapshot::Loading,
    }
}

/// Release gpui's decoded copies of evicted images: the asset-system entry
/// AND the sprite-atlas tiles (`ImageSource::evict` — `remove_asset` alone
/// left the tiles resident forever). Pass the window being updated when
/// calling from a render path, since that window is detached from
/// `App::windows` during its own update. Cheap when nothing was evicted.
pub fn flush_evicted(mut window: Option<&mut gpui::Window>, cx: &mut gpui::App) {
    let evicted = std::mem::take(&mut cache().lock().unwrap().pending_free);
    for image in evicted {
        gpui::ImageSource::Image(image).evict(window.as_deref_mut(), cx);
    }
}

/// Claim the load for a source: `true` ⇒ the caller should start fetching now
/// (the entry is marked Loading so concurrent renders don't double-fetch).
/// Errored sources hand out a retry only after their backoff has elapsed.
pub fn begin_load(device_id: &str, path: &str) -> bool {
    let mut cache = cache().lock().unwrap();
    let entry = cache.map.entry(key(device_id, path));
    match entry {
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(CacheEntry::Loading { attempts: 0 });
            true
        }
        std::collections::hash_map::Entry::Occupied(mut o) => match o.get() {
            CacheEntry::Error { attempts, at }
                if at.elapsed() >= retry_delay(attempts.saturating_sub(1)) =>
            {
                let attempts = *attempts;
                o.insert(CacheEntry::Loading { attempts });
                true
            }
            _ => false,
        },
    }
}

pub fn store_loaded(device_id: &str, path: &str, name: SharedString, image: Arc<Image>) {
    cache()
        .lock()
        .unwrap()
        .insert_loaded(key(device_id, path), CachedAttachmentImage { name, image });
}

pub fn store_error(device_id: &str, path: &str) {
    let mut cache = cache().lock().unwrap();
    let attempts = match cache.map.get(&key(device_id, path)) {
        Some(CacheEntry::Loading { attempts }) => attempts + 1,
        Some(CacheEntry::Error { attempts, .. }) => *attempts,
        _ => 1,
    };
    cache.map.insert(
        key(device_id, path),
        CacheEntry::Error {
            attempts,
            at: Instant::now(),
        },
    );
}

/// Seed the cache after a successful upload (composer send path) so the just-
/// sent bubble's thumbnails render from local bytes instead of a round-trip.
pub fn seed_attachment(device_id: &str, path: &str, name: &str, image: Arc<Image>) {
    store_loaded(device_id, path, name.to_string().into(), image);
}

/// Resolve an inline-markdown image URL (`![alt](url)`) to an engine-readable
/// file path. `file://` URLs and absolute paths pass through; relative paths
/// join the chat's cwd; anything else with a scheme (http, data, …) is not
/// inline-loadable — the app registers no HTTP client — and stays a link.
pub fn inline_image_path(url: &str, cwd: Option<&str>) -> Option<String> {
    let url = url.trim();
    if let Some(path) = url.strip_prefix("file://") {
        return path.starts_with('/').then(|| path.to_string());
    }
    if url.starts_with('/') {
        return Some(url.to_string());
    }
    if url.is_empty()
        // A scheme before the first slash (`https:`, `data:`, the mend
        // sentinel `comet:`) means remote/synthetic.
        || url.split('/').next().is_some_and(|head| head.contains(':'))
        // `~` only means home on the DEVICE OWNING the file; never guess it.
        || url.starts_with('~')
    {
        return None;
    }
    cwd.map(|cwd| format!("{}/{url}", cwd.trim_end_matches('/')))
}

// ---------------------------------------------------------------------------
// Preview lightbox (attachment-ui.tsx AttachmentPreviewDialog)
// ---------------------------------------------------------------------------

/// A full-size preview target (staged strip or transcript thumbnail).
#[derive(Clone)]
pub struct PreviewImage {
    pub name: SharedString,
    pub image: Arc<Image>,
}

/// The bare lightbox: dim scrim, the image at ≤85vh/90vw, the file name under
/// it. Any click closes (the whole dialog is the close button, as in the
/// original's `cursor-zoom-out` figure).
pub fn lightbox(
    viewport: Size<gpui::Pixels>,
    preview: &PreviewImage,
    on_close: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let max_h = px(f32::from(viewport.height) * 0.85);
    let max_w = px(f32::from(viewport.width) * 0.9);
    gpui::deferred(
        gpui::anchored()
            .position(gpui::point(px(0.0), px(0.0)))
            .child(
                div()
                    .id("attachment-lightbox")
                    .occlude()
                    .w(viewport.width)
                    .h(viewport.height)
                    .bg(crate::popover::scrim_alpha(0.7))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(12.0))
                    .cursor_pointer()
                    .on_click(move |_, window, cx| on_close(window, cx))
                    .child(
                        img(preview.image.clone())
                            .object_fit(ObjectFit::Contain)
                            .max_h(max_h)
                            .max_w(max_w)
                            .rounded(px(6.0))
                            .shadow_2xl(),
                    )
                    .child(
                        div()
                            .max_w(max_w)
                            .overflow_hidden()
                            .text_size(px(11.0))
                            .text_color(ink(0.45))
                            .child(preview.name.clone()),
                    ),
            ),
    )
    .priority(3)
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_attachments_round_trips_through_parse() {
        let paths = vec!["/data/uploads/ab-cat.png".to_string(), "/x/dog.jpg".into()];
        let content = with_attachments("look at these", &paths);
        let parsed = parse_user_message_attachments(&content);
        assert_eq!(parsed.text, "look at these");
        assert_eq!(parsed.attachments.len(), 2);
        assert!(parsed.file_attachments.is_empty());
        assert_eq!(parsed.attachments[0].path, "/data/uploads/ab-cat.png");
        assert_eq!(parsed.attachments[0].name, "ab-cat.png");
        assert_eq!(parsed.attachments[1].name, "dog.jpg");
        assert_eq!(parsed.attachments[0].id, "0:/data/uploads/ab-cat.png");
    }

    #[test]
    fn image_only_send_hides_placeholder_body() {
        let content = with_attachments("", &["/a/b.png".to_string()]);
        assert!(content.starts_with(ATTACHMENT_ONLY_TEXT));
        let parsed = parse_user_message_attachments(&content);
        assert_eq!(parsed.text, "");
        assert_eq!(parsed.attachments.len(), 1);
    }

    #[test]
    fn files_and_images_round_trip_as_distinct_attachment_types() {
        let content = with_attachment_files(
            "",
            &["/data/uploads/shot.png".to_string()],
            &["/data/uploads/example resumes/George Resume.pdf".to_string()],
        );
        let parsed = parse_user_message_attachments(&content);
        assert!(parsed.text.is_empty());
        assert_eq!(parsed.attachments[0].name, "shot.png");
        assert_eq!(parsed.file_attachments[0].name, "George Resume.pdf");

        let file_only = with_attachment_files(
            "",
            &[],
            &["/data/uploads/example resumes/George Resume.pdf".to_string()],
        );
        assert!(file_only.starts_with(FILE_ATTACHMENT_ONLY_TEXT));
        let parsed = parse_user_message_attachments(&file_only);
        assert!(parsed.text.is_empty());
        assert!(parsed.attachments.is_empty());
        assert_eq!(parsed.file_attachments.len(), 1);
    }

    #[test]
    fn legacy_pasted_text_refs_still_parse_as_files() {
        let content = concat!(
            "See the attached text.",
            "\n\nAttached text (local files — open them to read):",
            "\n- /data/uploads/pasted-text.txt"
        );
        let parsed = parse_user_message_attachments(content);
        assert!(parsed.text.is_empty());
        assert_eq!(parsed.file_attachments[0].name, "pasted-text.txt");
    }

    #[test]
    fn plain_text_passes_through_unchanged() {
        assert_eq!(with_attachments("hello", &[]), "hello");
        let parsed = parse_user_message_attachments("hello\n\nno images here");
        assert!(parsed.attachments.is_empty());
        assert!(parsed.file_attachments.is_empty());
        assert_eq!(parsed.text, "hello\n\nno images here");
    }

    #[test]
    fn marker_is_case_insensitive_and_requires_ref_lines() {
        let parsed = parse_user_message_attachments(
            "hi\n\nATTACHED IMAGES (local files — open them to view):\n- /p/q.png",
        );
        assert_eq!(parsed.attachments.len(), 1);
        // A trailer with no valid `- path` lines is left as plain text.
        let empty = parse_user_message_attachments(
            "hi\n\nAttached images (local files — open them to view):\nnothing",
        );
        assert!(empty.attachments.is_empty());
        assert!(empty.text.contains("Attached images"));
    }

    #[test]
    fn rail_text_summarizes_attachment_only_sends() {
        let one = with_attachments("", &["/a/b.png".to_string()]);
        assert_eq!(user_message_rail_text(&one), "Attached image");
        let two = with_attachments("", &["/a/b.png".to_string(), "/c/d.png".into()]);
        assert_eq!(user_message_rail_text(&two), "2 attached images");
        let file = with_attachment_files("", &[], &["/a/George Resume.pdf".to_string()]);
        assert_eq!(user_message_rail_text(&file), "Attached file");
        let with_text = with_attachments("fix this", &["/a/b.png".to_string()]);
        assert_eq!(user_message_rail_text(&with_text), "fix this");
        assert_eq!(user_message_rail_text("plain"), "plain");
    }

    #[test]
    fn ensure_extension_matches_browser_heuristic() {
        assert_eq!(ensure_extension("shot.png", ImageFormat::Png), "shot.png");
        assert_eq!(ensure_extension("image", ImageFormat::Png), "image.png");
        assert_eq!(
            ensure_extension("photo.j", ImageFormat::Jpeg),
            "photo.j.jpg"
        );
        assert_eq!(
            ensure_extension("archive.tar.gz", ImageFormat::Png),
            "archive.tar.gz"
        );
    }

    #[test]
    fn supported_formats_match_engine_jail() {
        for (ext, expect) in [
            ("png", Some(ImageFormat::Png)),
            ("JPG", Some(ImageFormat::Jpeg)),
            ("webp", Some(ImageFormat::Webp)),
            ("svg", Some(ImageFormat::Svg)),
            ("ico", None),
            ("txt", None),
        ] {
            assert_eq!(
                format_by_extension(Path::new(&format!("f.{ext}"))),
                expect,
                "ext {ext}"
            );
        }
    }

    #[test]
    fn stages_non_image_file_from_directory_with_spaces() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("example resumes");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("George Resume.pdf");
        std::fs::write(&path, b"%PDF-1.7 resume").unwrap();

        let attachment = stage_file(&path).expect("PDF should stage as a file attachment");
        assert_eq!(attachment.name, "George Resume.pdf");
        assert_eq!(attachment.display_name, "George Resume.pdf");
        assert_eq!(attachment.bytes(), b"%PDF-1.7 resume");
        assert!(attachment.image().is_none());
    }

    #[test]
    fn stages_folder_recursively_with_relative_labels() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("example resumes");
        let nested = root.join("engineering");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("George Resume.pdf"), b"george").unwrap();
        std::fs::write(nested.join("Borello Tina.docx"), b"tina").unwrap();

        let attachments = stage_selected_path(&root).expect("folder should stage recursively");
        assert_eq!(attachments.len(), 2);
        assert_eq!(
            attachments
                .iter()
                .map(|attachment| attachment.display_name.as_str())
                .collect::<Vec<_>>(),
            [
                "example resumes/George Resume.pdf",
                "example resumes/engineering/Borello Tina.docx",
            ]
        );
        assert_eq!(attachments[0].display_basename(), "George Resume.pdf");
        assert!(attachments[1].name.starts_with(FOLDER_NAME_PREFIX));
        assert!(attachments[1].name.ends_with(".docx"));
        assert_eq!(attachments[0].bytes(), b"george");
        assert_eq!(attachments[1].bytes(), b"tina");
    }

    #[test]
    fn empty_folder_returns_visible_error() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("empty folder");
        std::fs::create_dir(&root).unwrap();
        let error = match stage_selected_path(&root) {
            Ok(_) => panic!("empty folder should fail"),
            Err(error) => error,
        };
        assert_eq!(error, "empty folder contains no regular files.");
    }

    #[test]
    fn uploaded_folder_names_restore_relative_labels() {
        let label = "example resumes/engineering/Borello Tina.docx";
        let upload_name = folder_upload_name(label).expect("folder label should fit");
        assert_eq!(
            name_from_path(&format!("/uploads/877c9e22-{upload_name}")),
            label
        );
    }

    #[test]
    fn retry_ladder_is_2s_doubling_capped_at_15s() {
        assert_eq!(retry_delay(0), Duration::from_millis(2_000));
        assert_eq!(retry_delay(1), Duration::from_millis(4_000));
        assert_eq!(retry_delay(2), Duration::from_millis(8_000));
        assert_eq!(retry_delay(3), Duration::from_millis(15_000));
        assert_eq!(retry_delay(9), Duration::from_millis(15_000));
    }

    #[test]
    fn inline_image_paths_resolve_local_sources_only() {
        assert_eq!(
            inline_image_path("/tmp/shot.png", None).as_deref(),
            Some("/tmp/shot.png")
        );
        assert_eq!(
            inline_image_path("file:///tmp/shot.png", None).as_deref(),
            Some("/tmp/shot.png")
        );
        // Relative paths anchor on the chat's cwd — and stay links without one.
        assert_eq!(
            inline_image_path("shots/a.png", Some("/repo/")).as_deref(),
            Some("/repo/shots/a.png")
        );
        assert_eq!(inline_image_path("shots/a.png", None), None);
        // Remote/synthetic schemes and home-relative guesses never load.
        assert_eq!(
            inline_image_path("https://x.dev/i.png", Some("/repo")),
            None
        );
        assert_eq!(inline_image_path("data:image/png;base64,xx", None), None);
        assert_eq!(inline_image_path("comet:pending-link", Some("/repo")), None);
        assert_eq!(inline_image_path("~/shot.png", Some("/repo")), None);
        assert_eq!(inline_image_path("", Some("/repo")), None);
    }
}
