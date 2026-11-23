//! Native macOS desktop computer-use MCP server for Crew.
//!
//! Launched via `Crew Engine mcp --computer-use` (or `zeron mcp --computer-use`)
//! and injected eagerly into Antigravity ACP sessions on macOS. Inherits the
//! host app bundle's Accessibility and Screen Recording permissions and exposes
//! three focused tools:
//!
//! - `computer_displays`: inspect active displays and macOS permission state.
//! - `computer_screenshot`: capture a scaled screenshot (default <= 1280x896)
//!   and establish the coordinate space for subsequent input actions.
//! - `computer_action`: execute one or a batch of desktop input actions
//!   (`click`, `double_click`, `right_click`, `move`, `drag`, `type`,
//!   `keypress`, `scroll`, `wait`, `screenshot`) in screenshot pixel
//!   coordinates and return an updated screenshot.

#[cfg(target_os = "macos")]
mod macos;

use std::borrow::Cow;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::{self, FilterType};
use image::{ColorType, ImageEncoder, RgbaImage};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::jsonrpc::{McpHandler, ToolOutput, serve_stdio_handler};

const DEFAULT_MAX_WIDTH: u32 = 1280;
const DEFAULT_MAX_HEIGHT: u32 = 896;
const MIN_MAX_DIM: u32 = 64;
const MAX_ALLOWED_WIDTH: u32 = 1920;
const MAX_ALLOWED_HEIGHT: u32 = 1280;
const MAX_PNG_BYTES: usize = 1_572_864; // 1.5 MiB -> JPEG fallback
const MAX_BATCH_ACTIONS: usize = 25;
const MAX_WAIT_MS: u64 = 10_000;
const DEFAULT_SETTLE_MS: u64 = 150;
const MAX_SETTLE_MS: u64 = 5_000;

const INSTRUCTIONS: &str = "\
Crew native desktop computer-use tools for controlling this Mac.\n\
Always call `computer_screenshot` first to inspect the screen and establish the \
pixel coordinate space (0..width, 0..height) before issuing pointer coordinates \
with `computer_action`. Coordinates passed to `computer_action` are always in \
the most recent screenshot's pixel space and are mapped accurately onto macOS \
display points (including Retina displays and multi-monitor layouts).\n\
You may pass either a single action (`action` + fields) or a multi-step batch \
(`actions: [...]`) to `computer_action`; by default it captures and returns a \
fresh screenshot after the actions complete.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionStatus {
    pub accessibility: bool,
    pub screen_recording: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DisplayInfo {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Modifier {
    Shift,
    Control,
    Alt,
    Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyCodeSpec {
    Virtual(u16),
    Unicode(char),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ResolvedAction {
    Move {
        x: f64,
        y: f64,
    },
    Click {
        x: f64,
        y: f64,
        button: MouseButton,
        clicks: u8,
        modifiers: Vec<Modifier>,
    },
    Drag {
        path: Vec<(f64, f64)>,
        button: MouseButton,
        modifiers: Vec<Modifier>,
    },
    Scroll {
        x: f64,
        y: f64,
        scroll_x: i32,
        scroll_y: i32,
        modifiers: Vec<Modifier>,
    },
    Type {
        text: String,
    },
    Keypress {
        modifiers: Vec<Modifier>,
        key: Option<KeyCodeSpec>,
    },
    Wait {
        duration_ms: u64,
    },
}

pub(crate) trait ComputerBackend: Send + Sync + 'static {
    fn permissions(&self) -> PermissionStatus;
    fn list_displays(&self) -> Result<Vec<DisplayInfo>, String>;
    fn capture_display(&self, display: &DisplayInfo) -> Result<RgbaImage, String>;
    fn execute_action(&self, action: &ResolvedAction) -> Result<(), String>;
}

#[cfg(not(target_os = "macos"))]
#[derive(Debug, Default)]
struct UnsupportedBackend;

#[cfg(not(target_os = "macos"))]
impl ComputerBackend for UnsupportedBackend {
    fn permissions(&self) -> PermissionStatus {
        PermissionStatus {
            accessibility: false,
            screen_recording: false,
        }
    }

    fn list_displays(&self) -> Result<Vec<DisplayInfo>, String> {
        Err("Crew computer use is currently supported on macOS only".into())
    }

    fn capture_display(&self, _display: &DisplayInfo) -> Result<RgbaImage, String> {
        Err("Crew computer use is currently supported on macOS only".into())
    }

    fn execute_action(&self, _action: &ResolvedAction) -> Result<(), String> {
        Err("Crew computer use is currently supported on macOS only".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DisplaySelector {
    Primary,
    All,
    Id(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct FrameDisplay {
    id: u32,
    is_primary: bool,
    logical_x: i32,
    logical_y: i32,
    logical_width: u32,
    logical_height: u32,
    pixel_x: u32,
    pixel_y: u32,
    pixel_width: u32,
    pixel_height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FrameGeometry {
    selector: DisplaySelector,
    width: u32,
    height: u32,
    max_width: u32,
    max_height: u32,
    displays: Vec<FrameDisplay>,
}

impl FrameGeometry {
    fn map_point(&self, x: i32, y: i32) -> Result<(f64, f64), String> {
        if x < 0 || y < 0 {
            return Err(format!(
                "screenshot coordinate ({x}, {y}) must be non-negative"
            ));
        }
        let ux = x as u32;
        let uy = y as u32;
        if ux >= self.width || uy >= self.height {
            return Err(format!(
                "screenshot coordinate ({x}, {y}) is outside the latest {}x{} frame",
                self.width, self.height
            ));
        }
        let frame = self
            .displays
            .iter()
            .find(|d| {
                ux >= d.pixel_x
                    && ux < d.pixel_x + d.pixel_width
                    && uy >= d.pixel_y
                    && uy < d.pixel_y + d.pixel_height
            })
            .ok_or_else(|| {
                format!(
                    "screenshot coordinate ({x}, {y}) falls in dead space outside active displays"
                )
            })?;
        let rel_x = (f64::from(ux - frame.pixel_x) + 0.5) / f64::from(frame.pixel_width);
        let rel_y = (f64::from(uy - frame.pixel_y) + 0.5) / f64::from(frame.pixel_height);
        let gx = f64::from(frame.logical_x) + rel_x * f64::from(frame.logical_width);
        let gy = f64::from(frame.logical_y) + rel_y * f64::from(frame.logical_height);
        Ok((gx, gy))
    }
}

#[derive(Debug, Clone)]
struct EncodedScreenshot {
    mime_type: &'static str,
    base64_data: String,
    byte_len: usize,
    geometry: FrameGeometry,
}

#[derive(Debug, Clone)]
enum ParsedAction {
    Screenshot,
    Move {
        x: i32,
        y: i32,
    },
    Click {
        x: i32,
        y: i32,
        button: MouseButton,
        clicks: u8,
        modifiers: Vec<Modifier>,
    },
    Drag {
        path: Vec<(i32, i32)>,
        button: MouseButton,
        modifiers: Vec<Modifier>,
    },
    Scroll {
        x: i32,
        y: i32,
        scroll_x: i32,
        scroll_y: i32,
        modifiers: Vec<Modifier>,
    },
    Type {
        text: String,
    },
    Keypress {
        modifiers: Vec<Modifier>,
        key: Option<KeyCodeSpec>,
    },
    Wait {
        duration_ms: u64,
    },
}

struct SessionState {
    last_frame: Option<FrameGeometry>,
}

pub(crate) struct ComputerUseServer<B: ComputerBackend> {
    backend: Arc<B>,
    state: Mutex<SessionState>,
}

impl<B: ComputerBackend> ComputerUseServer<B> {
    pub(crate) fn new(backend: Arc<B>) -> Self {
        Self {
            backend,
            state: Mutex::new(SessionState { last_frame: None }),
        }
    }

    async fn capture(
        &self,
        selector: DisplaySelector,
        max_width: u32,
        max_height: u32,
    ) -> Result<EncodedScreenshot, String> {
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || {
            capture_and_encode(backend.as_ref(), selector, max_width, max_height)
        })
        .await
        .map_err(|err| format!("screenshot task failed: {err}"))?
    }
}

fn capture_and_encode<B: ComputerBackend>(
    backend: &B,
    selector: DisplaySelector,
    max_width: u32,
    max_height: u32,
) -> Result<EncodedScreenshot, String> {
    let all_displays = backend.list_displays()?;
    let selected: Vec<DisplayInfo> = match &selector {
        DisplaySelector::Primary => {
            let primary = all_displays
                .iter()
                .find(|d| d.is_primary)
                .or_else(|| all_displays.first())
                .cloned()
                .ok_or_else(|| "no active display found".to_owned())?;
            vec![primary]
        }
        DisplaySelector::All => all_displays.clone(),
        DisplaySelector::Id(id) => {
            let found = all_displays
                .iter()
                .find(|d| d.id == *id)
                .cloned()
                .ok_or_else(|| {
                    let ids: Vec<String> = all_displays.iter().map(|d| d.id.to_string()).collect();
                    format!("display {id} not found (active displays: {})", ids.join(", "))
                })?;
            vec![found]
        }
    };

    let min_x = selected.iter().map(|d| i64::from(d.x)).min().unwrap_or(0);
    let min_y = selected.iter().map(|d| i64::from(d.y)).min().unwrap_or(0);
    let max_x = selected
        .iter()
        .map(|d| i64::from(d.x) + i64::from(d.width))
        .max()
        .unwrap_or(1);
    let max_y = selected
        .iter()
        .map(|d| i64::from(d.y) + i64::from(d.height))
        .max()
        .unwrap_or(1);
    let virtual_w = (max_x - min_x).max(1) as f64;
    let virtual_h = (max_y - min_y).max(1) as f64;

    let scale = (f64::from(max_width) / virtual_w)
        .min(f64::from(max_height) / virtual_h)
        .min(1.0);

    let canvas_w = ((virtual_w * scale).round() as u32).max(1);
    let canvas_h = ((virtual_h * scale).round() as u32).max(1);

    let (canvas, frame_displays) = if selected.len() == 1 {
        let d = &selected[0];
        let raw = backend.capture_display(d)?;
        let resized = if raw.width() == canvas_w && raw.height() == canvas_h {
            raw
        } else {
            imageops::resize(&raw, canvas_w, canvas_h, FilterType::Triangle)
        };
        let fd = FrameDisplay {
            id: d.id,
            is_primary: d.is_primary,
            logical_x: d.x,
            logical_y: d.y,
            logical_width: d.width,
            logical_height: d.height,
            pixel_x: 0,
            pixel_y: 0,
            pixel_width: canvas_w,
            pixel_height: canvas_h,
        };
        (resized, vec![fd])
    } else {
        let mut canvas = RgbaImage::from_pixel(canvas_w, canvas_h, image::Rgba([0, 0, 0, 255]));
        let mut frames = Vec::with_capacity(selected.len());
        for d in &selected {
            let raw = backend.capture_display(d)?;
            let px = (((i64::from(d.x) - min_x) as f64) * scale).round().max(0.0) as u32;
            let py = (((i64::from(d.y) - min_y) as f64) * scale).round().max(0.0) as u32;
            let pw = ((f64::from(d.width) * scale).round() as u32)
                .max(1)
                .min(canvas_w.saturating_sub(px).max(1));
            let ph = ((f64::from(d.height) * scale).round() as u32)
                .max(1)
                .min(canvas_h.saturating_sub(py).max(1));
            let resized = imageops::resize(&raw, pw, ph, FilterType::Triangle);
            imageops::overlay(&mut canvas, &resized, i64::from(px), i64::from(py));
            frames.push(FrameDisplay {
                id: d.id,
                is_primary: d.is_primary,
                logical_x: d.x,
                logical_y: d.y,
                logical_width: d.width,
                logical_height: d.height,
                pixel_x: px,
                pixel_y: py,
                pixel_width: pw,
                pixel_height: ph,
            });
        }
        (canvas, frames)
    };

    let (mime_type, bytes) = encode_image(&canvas)?;
    let byte_len = bytes.len();
    let base64_data = base64::engine::general_purpose::STANDARD.encode(bytes);

    Ok(EncodedScreenshot {
        mime_type,
        base64_data,
        byte_len,
        geometry: FrameGeometry {
            selector,
            width: canvas_w,
            height: canvas_h,
            max_width,
            max_height,
            displays: frame_displays,
        },
    })
}

fn encode_image(img: &RgbaImage) -> Result<(&'static str, Vec<u8>), String> {
    let mut png_bytes = Vec::new();
    PngEncoder::new(&mut png_bytes)
        .write_image(img.as_raw(), img.width(), img.height(), ColorType::Rgba8.into())
        .map_err(|err| format!("failed to encode screenshot PNG: {err}"))?;
    if png_bytes.len() <= MAX_PNG_BYTES {
        return Ok(("image/png", png_bytes));
    }

    let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
    let mut jpeg_bytes = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(Cursor::new(&mut jpeg_bytes), 85);
    encoder
        .encode(rgb.as_raw(), rgb.width(), rgb.height(), ColorType::Rgb8.into())
        .map_err(|err| format!("failed to encode screenshot JPEG fallback: {err}"))?;
    Ok(("image/jpeg", jpeg_bytes))
}

fn screenshot_tool_output(shot: &EncodedScreenshot, extra: Option<Value>) -> ToolOutput {
    let mut meta = json!({
        "width": shot.geometry.width,
        "height": shot.geometry.height,
        "mimeType": shot.mime_type,
        "bytes": shot.byte_len,
        "displays": shot.geometry.displays,
    });
    if let Some(Value::Object(map)) = extra {
        if let Some(target) = meta.as_object_mut() {
            for (k, v) in map {
                target.insert(k, v);
            }
        }
    }
    let text = serde_json::to_string_pretty(&meta).unwrap_or_default();
    ToolOutput {
        content: vec![
            json!({ "type": "text", "text": text }),
            json!({
                "type": "image",
                "data": shot.base64_data,
                "mimeType": shot.mime_type,
            }),
        ],
        structured: Some(meta),
    }
}

fn parse_display_selector(value: Option<&Value>) -> Result<DisplaySelector, String> {
    match value {
        None | Some(Value::Null) => Ok(DisplaySelector::Primary),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("primary") {
                Ok(DisplaySelector::Primary)
            } else if trimmed.eq_ignore_ascii_case("all") {
                Ok(DisplaySelector::All)
            } else {
                trimmed
                    .parse::<u32>()
                    .map(DisplaySelector::Id)
                    .map_err(|_| {
                        format!(
                            "invalid display selector {s:?}: expected \"primary\", \"all\", or a numeric display ID"
                        )
                    })
            }
        }
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .map(DisplaySelector::Id)
            .ok_or_else(|| format!("invalid numeric display selector: {n}")),
        Some(other) => Err(format!("invalid display selector: {other}")),
    }
}

fn parse_dim(value: Option<&Value>, default: u32, max: u32, field: &str) -> Result<u32, String> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => {
            let v = n
                .as_u64()
                .and_then(|u| u32::try_from(u).ok())
                .ok_or_else(|| format!("{field} must be a positive integer"))?;
            Ok(v.clamp(MIN_MAX_DIM, max))
        }
        Some(_) => Err(format!("{field} must be a positive integer")),
    }
}

fn parse_i32(obj: &serde_json::Map<String, Value>, field: &str) -> Result<i32, String> {
    let val = obj
        .get(field)
        .ok_or_else(|| format!("missing required integer field `{field}`"))?;
    val.as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .or_else(|| {
            val.as_f64()
                .filter(|f| f.is_finite() && f.fract() == 0.0)
                .map(|f| f as i32)
        })
        .ok_or_else(|| format!("field `{field}` must be an integer"))
}

fn parse_optional_i32(
    obj: &serde_json::Map<String, Value>,
    field: &str,
    default: i32,
) -> Result<i32, String> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(_) => parse_i32(obj, field),
    }
}

fn parse_button(value: Option<&Value>, default: MouseButton) -> Result<MouseButton, String> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "left" => Ok(MouseButton::Left),
            "right" => Ok(MouseButton::Right),
            "middle" | "wheel" | "center" => Ok(MouseButton::Middle),
            other => Err(format!(
                "invalid mouse button {other:?}: expected \"left\", \"right\", or \"middle\""
            )),
        },
        Some(_) => Err("field `button` must be a string".into()),
    }
}

fn parse_modifier(name: &str) -> Option<Modifier> {
    match name.trim().to_ascii_uppercase().as_str() {
        "SHIFT" => Some(Modifier::Shift),
        "CTRL" | "CONTROL" => Some(Modifier::Control),
        "ALT" | "OPTION" | "OPT" => Some(Modifier::Alt),
        "CMD" | "COMMAND" | "META" | "SUPER" | "GUI" => Some(Modifier::Command),
        _ => None,
    }
}

fn parse_modifiers_opt(value: Option<&Value>) -> Result<Vec<Modifier>, String> {
    let Some(val) = value else {
        return Ok(Vec::new());
    };
    if val.is_null() {
        return Ok(Vec::new());
    }
    let arr = val
        .as_array()
        .ok_or_else(|| "field `keys` must be an array of key names".to_owned())?;
    let mut modifiers = Vec::new();
    for item in arr {
        let name = item
            .as_str()
            .ok_or_else(|| "each entry in `keys` must be a string".to_owned())?;
        let m = parse_modifier(name).ok_or_else(|| {
            format!(
                "pointer actions only accept modifier keys in `keys` (SHIFT, CTRL, ALT/OPTION, CMD/META), got {name:?}"
            )
        })?;
        if !modifiers.contains(&m) {
            modifiers.push(m);
        }
    }
    Ok(modifiers)
}

pub(crate) fn parse_keypress(keys: &[String]) -> Result<(Vec<Modifier>, Option<KeyCodeSpec>), String> {
    if keys.is_empty() {
        return Err("keypress requires at least one key in `keys`".into());
    }
    let mut modifiers = Vec::new();
    let mut primary: Option<KeyCodeSpec> = None;
    for raw in keys {
        // Allow compact chord syntax like "CMD+SHIFT+P" in a single string too.
        let parts: Vec<&str> = if raw.len() > 1 && raw.contains('+') {
            raw.split('+').map(str::trim).filter(|s| !s.is_empty()).collect()
        } else {
            vec![raw.trim()]
        };
        for part in parts {
            if part.is_empty() {
                continue;
            }
            if let Some(m) = parse_modifier(part) {
                if !modifiers.contains(&m) {
                    modifiers.push(m);
                }
            } else {
                if primary.is_some() {
                    return Err(format!(
                        "keypress accepts at most one non-modifier key per chord (got extra {part:?})"
                    ));
                }
                primary = Some(parse_key_spec(part)?);
            }
        }
    }
    if modifiers.is_empty() && primary.is_none() {
        return Err("keypress requires at least one valid key".into());
    }
    Ok((modifiers, primary))
}

fn parse_key_spec(key: &str) -> Result<KeyCodeSpec, String> {
    let upper = key.to_ascii_uppercase();
    let code = match upper.as_str() {
        "A" => Some(0x00),
        "S" => Some(0x01),
        "D" => Some(0x02),
        "F" => Some(0x03),
        "H" => Some(0x04),
        "G" => Some(0x05),
        "Z" => Some(0x06),
        "X" => Some(0x07),
        "C" => Some(0x08),
        "V" => Some(0x09),
        "B" => Some(0x0B),
        "Q" => Some(0x0C),
        "W" => Some(0x0D),
        "E" => Some(0x0E),
        "R" => Some(0x0F),
        "Y" => Some(0x10),
        "T" => Some(0x11),
        "1" => Some(0x12),
        "2" => Some(0x13),
        "3" => Some(0x14),
        "4" => Some(0x15),
        "6" => Some(0x16),
        "5" => Some(0x17),
        "=" | "EQUAL" | "EQUALS" => Some(0x18),
        "9" => Some(0x19),
        "7" => Some(0x1A),
        "-" | "MINUS" => Some(0x1B),
        "8" => Some(0x1C),
        "0" => Some(0x1D),
        "]" | "RIGHTBRACKET" => Some(0x1E),
        "O" => Some(0x1F),
        "U" => Some(0x20),
        "[" | "LEFTBRACKET" => Some(0x21),
        "I" => Some(0x22),
        "P" => Some(0x23),
        "RETURN" | "ENTER" => Some(0x24),
        "L" => Some(0x25),
        "J" => Some(0x26),
        "'" | "QUOTE" => Some(0x27),
        "K" => Some(0x28),
        ";" | "SEMICOLON" => Some(0x29),
        "\\" | "BACKSLASH" => Some(0x2A),
        "," | "COMMA" => Some(0x2B),
        "/" | "SLASH" => Some(0x2C),
        "N" => Some(0x2D),
        "M" => Some(0x2E),
        "." | "PERIOD" => Some(0x2F),
        "TAB" => Some(0x30),
        " " | "SPACE" => Some(0x31),
        "`" | "GRAVE" | "BACKQUOTE" => Some(0x32),
        "BACKSPACE" | "DELETE" | "BS" => Some(0x33),
        "ESC" | "ESCAPE" => Some(0x35),
        "HOME" => Some(0x73),
        "PAGEUP" | "PAGE_UP" | "PGUP" => Some(0x74),
        "FORWARDDELETE" | "FORWARD_DELETE" | "DEL" => Some(0x75),
        "END" => Some(0x77),
        "PAGEDOWN" | "PAGE_DOWN" | "PGDN" => Some(0x79),
        "LEFT" | "ARROWLEFT" | "ARROW_LEFT" => Some(0x7B),
        "RIGHT" | "ARROWRIGHT" | "ARROW_RIGHT" => Some(0x7C),
        "DOWN" | "ARROWDOWN" | "ARROW_DOWN" => Some(0x7D),
        "UP" | "ARROWUP" | "ARROW_UP" => Some(0x7E),
        "F1" => Some(0x7A),
        "F2" => Some(0x78),
        "F3" => Some(0x63),
        "F4" => Some(0x76),
        "F5" => Some(0x60),
        "F6" => Some(0x61),
        "F7" => Some(0x62),
        "F8" => Some(0x64),
        "F9" => Some(0x65),
        "F10" => Some(0x6D),
        "F11" => Some(0x67),
        "F12" => Some(0x6F),
        _ => None,
    };
    if let Some(vk) = code {
        return Ok(KeyCodeSpec::Virtual(vk));
    }
    let mut chars = key.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        return Ok(KeyCodeSpec::Unicode(ch));
    }
    Err(format!("unknown key name {key:?}"))
}

fn parse_action_object(obj: &serde_json::Map<String, Value>) -> Result<ParsedAction, String> {
    let kind = obj
        .get("action")
        .or_else(|| obj.get("type"))
        .and_then(Value::as_str)
        .ok_or_else(|| "each action must specify `action` (or `type`) as a string".to_owned())?;
    match kind.trim().to_ascii_lowercase().as_str() {
        "screenshot" => Ok(ParsedAction::Screenshot),
        "move" => Ok(ParsedAction::Move {
            x: parse_i32(obj, "x")?,
            y: parse_i32(obj, "y")?,
        }),
        "click" => Ok(ParsedAction::Click {
            x: parse_i32(obj, "x")?,
            y: parse_i32(obj, "y")?,
            button: parse_button(obj.get("button"), MouseButton::Left)?,
            clicks: 1,
            modifiers: parse_modifiers_opt(obj.get("keys"))?,
        }),
        "double_click" => Ok(ParsedAction::Click {
            x: parse_i32(obj, "x")?,
            y: parse_i32(obj, "y")?,
            button: parse_button(obj.get("button"), MouseButton::Left)?,
            clicks: 2,
            modifiers: parse_modifiers_opt(obj.get("keys"))?,
        }),
        "right_click" => Ok(ParsedAction::Click {
            x: parse_i32(obj, "x")?,
            y: parse_i32(obj, "y")?,
            button: MouseButton::Right,
            clicks: 1,
            modifiers: parse_modifiers_opt(obj.get("keys"))?,
        }),
        "drag" => {
            let arr = obj
                .get("path")
                .and_then(Value::as_array)
                .ok_or_else(|| "drag requires `path` array of at least 2 {x, y} points".to_owned())?;
            if arr.len() < 2 {
                return Err("drag `path` requires at least 2 points".into());
            }
            let mut path = Vec::with_capacity(arr.len());
            for pt in arr {
                let map = pt
                    .as_object()
                    .ok_or_else(|| "each drag `path` point must be an object {x, y}".to_owned())?;
                path.push((parse_i32(map, "x")?, parse_i32(map, "y")?));
            }
            Ok(ParsedAction::Drag {
                path,
                button: parse_button(obj.get("button"), MouseButton::Left)?,
                modifiers: parse_modifiers_opt(obj.get("keys"))?,
            })
        }
        "scroll" => Ok(ParsedAction::Scroll {
            x: parse_i32(obj, "x")?,
            y: parse_i32(obj, "y")?,
            scroll_x: parse_optional_i32(obj, "scroll_x", 0)?,
            scroll_y: parse_optional_i32(obj, "scroll_y", 0)?,
            modifiers: parse_modifiers_opt(obj.get("keys"))?,
        }),
        "type" => {
            let text = obj
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "type action requires a string `text` field".to_owned())?;
            if text.is_empty() {
                return Err("type action `text` must not be empty".into());
            }
            Ok(ParsedAction::Type {
                text: text.to_owned(),
            })
        }
        "keypress" => {
            let keys_val = obj
                .get("keys")
                .ok_or_else(|| "keypress action requires `keys` array of key names".to_owned())?;
            let keys: Vec<String> = match keys_val {
                Value::Array(arr) => arr
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| "each entry in `keys` must be a string".to_owned())
                    })
                    .collect::<Result<_, _>>()?,
                Value::String(s) => vec![s.clone()],
                _ => return Err("keypress `keys` must be an array of strings".into()),
            };
            let (modifiers, key) = parse_keypress(&keys)?;
            Ok(ParsedAction::Keypress { modifiers, key })
        }
        "wait" => {
            let duration_ms = match obj.get("duration_ms") {
                None | Some(Value::Null) => 1000,
                Some(Value::Number(n)) => n
                    .as_u64()
                    .ok_or_else(|| "wait `duration_ms` must be a non-negative integer".to_owned())?,
                Some(_) => return Err("wait `duration_ms` must be a non-negative integer".into()),
            };
            Ok(ParsedAction::Wait {
                duration_ms: duration_ms.min(MAX_WAIT_MS),
            })
        }
        other => Err(format!(
            "unknown computer action {other:?}: expected click, double_click, right_click, move, drag, type, keypress, scroll, wait, or screenshot"
        )),
    }
}

fn resolve_actions(
    actions: &[ParsedAction],
    frame: Option<&FrameGeometry>,
) -> Result<(Vec<ResolvedAction>, bool), String> {
    let mut resolved = Vec::with_capacity(actions.len());
    let mut explicit_screenshot = false;
    let require_frame = |action_name: &str| -> Result<&FrameGeometry, String> {
        frame.ok_or_else(|| {
            format!(
                "call `computer_screenshot` before `{action_name}` so coordinates map to the current screen"
            )
        })
    };

    for action in actions {
        match action {
            ParsedAction::Screenshot => {
                explicit_screenshot = true;
            }
            ParsedAction::Move { x, y } => {
                let (gx, gy) = require_frame("move")?.map_point(*x, *y)?;
                resolved.push(ResolvedAction::Move { x: gx, y: gy });
            }
            ParsedAction::Click {
                x,
                y,
                button,
                clicks,
                modifiers,
            } => {
                let (gx, gy) = require_frame("click")?.map_point(*x, *y)?;
                resolved.push(ResolvedAction::Click {
                    x: gx,
                    y: gy,
                    button: *button,
                    clicks: *clicks,
                    modifiers: modifiers.clone(),
                });
            }
            ParsedAction::Drag {
                path,
                button,
                modifiers,
            } => {
                let frame = require_frame("drag")?;
                let mut mapped = Vec::with_capacity(path.len());
                for &(x, y) in path {
                    mapped.push(frame.map_point(x, y)?);
                }
                resolved.push(ResolvedAction::Drag {
                    path: mapped,
                    button: *button,
                    modifiers: modifiers.clone(),
                });
            }
            ParsedAction::Scroll {
                x,
                y,
                scroll_x,
                scroll_y,
                modifiers,
            } => {
                let (gx, gy) = require_frame("scroll")?.map_point(*x, *y)?;
                resolved.push(ResolvedAction::Scroll {
                    x: gx,
                    y: gy,
                    scroll_x: *scroll_x,
                    scroll_y: *scroll_y,
                    modifiers: modifiers.clone(),
                });
            }
            ParsedAction::Type { text } => {
                resolved.push(ResolvedAction::Type { text: text.clone() });
            }
            ParsedAction::Keypress { modifiers, key } => {
                resolved.push(ResolvedAction::Keypress {
                    modifiers: modifiers.clone(),
                    key: key.clone(),
                });
            }
            ParsedAction::Wait { duration_ms } => {
                resolved.push(ResolvedAction::Wait {
                    duration_ms: *duration_ms,
                });
            }
        }
    }
    Ok((resolved, explicit_screenshot))
}

#[async_trait]
impl<B: ComputerBackend> McpHandler for ComputerUseServer<B> {
    fn server_name(&self) -> &'static str {
        "computer_use"
    }

    fn instructions(&self) -> Cow<'static, str> {
        INSTRUCTIONS.into()
    }

    fn list(&self) -> Vec<Value> {
        let single_action_schema = json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "click",
                        "double_click",
                        "right_click",
                        "move",
                        "drag",
                        "type",
                        "keypress",
                        "scroll",
                        "wait",
                        "screenshot"
                    ],
                    "description": "Desktop action to perform."
                },
                "x": { "type": "integer", "description": "X pixel coordinate in the latest screenshot." },
                "y": { "type": "integer", "description": "Y pixel coordinate in the latest screenshot." },
                "button": {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "description": "Mouse button (default: left)."
                },
                "path": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer" },
                            "y": { "type": "integer" }
                        },
                        "required": ["x", "y"]
                    },
                    "description": "Waypoints for drag (>= 2 points in screenshot pixel coordinates)."
                },
                "text": { "type": "string", "description": "UTF-8 text to type (for action=type)." },
                "keys": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Key chord for keypress (e.g. [\"CMD\", \"L\"], [\"ENTER\"], [\"CMD\", \"SHIFT\", \"P\"]), or held modifier keys for pointer actions."
                },
                "scroll_x": { "type": "integer", "description": "Horizontal scroll pixels (positive scrolls right)." },
                "scroll_y": { "type": "integer", "description": "Vertical scroll pixels (positive scrolls content down)." },
                "duration_ms": { "type": "integer", "description": "Milliseconds to wait (for action=wait, default 1000, max 10000)." }
            },
            "required": ["action"]
        });

        vec![
            json!({
                "name": "computer_displays",
                "description": "List active displays (IDs, logical Quartz bounds, primary flag) and macOS Accessibility / Screen Recording permission status without prompting.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }),
            json!({
                "name": "computer_screenshot",
                "description": "Capture a screenshot of this Mac and establish the pixel coordinate space for `computer_action`. Returns an inline image plus frame dimensions and display bounds.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "display": {
                            "type": "string",
                            "description": "Display target: \"primary\" (default), \"all\" (composite of all displays), or a numeric display ID from `computer_displays`."
                        },
                        "max_width": {
                            "type": "integer",
                            "description": "Maximum output image width in pixels (default 1280, max 1920)."
                        },
                        "max_height": {
                            "type": "integer",
                            "description": "Maximum output image height in pixels (default 896, max 1280)."
                        }
                    }
                }
            }),
            json!({
                "name": "computer_action",
                "description": "Execute a desktop GUI action or ordered batch of actions (click, double_click, right_click, move, drag, type, keypress, scroll, wait) in the latest `computer_screenshot` pixel coordinate space, then capture and return an updated screenshot.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": [
                                "click",
                                "double_click",
                                "right_click",
                                "move",
                                "drag",
                                "type",
                                "keypress",
                                "scroll",
                                "wait",
                                "screenshot"
                            ],
                            "description": "Single action to execute (use either top-level `action` or `actions` array)."
                        },
                        "actions": {
                            "type": "array",
                            "items": single_action_schema,
                            "description": "Ordered batch of actions (up to 25) to execute sequentially before capturing the follow-up screenshot."
                        },
                        "x": { "type": "integer", "description": "X pixel coordinate in the latest screenshot." },
                        "y": { "type": "integer", "description": "Y pixel coordinate in the latest screenshot." },
                        "button": { "type": "string", "enum": ["left", "right", "middle"] },
                        "path": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "x": { "type": "integer" },
                                    "y": { "type": "integer" }
                                },
                                "required": ["x", "y"]
                            }
                        },
                        "text": { "type": "string" },
                        "keys": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "scroll_x": { "type": "integer" },
                        "scroll_y": { "type": "integer" },
                        "duration_ms": { "type": "integer" },
                        "capture_after": {
                            "type": "boolean",
                            "description": "Whether to capture and return an updated screenshot after executing actions (default true)."
                        },
                        "settle_ms": {
                            "type": "integer",
                            "description": "Milliseconds to wait after actions before taking the follow-up screenshot (default 150, max 5000)."
                        }
                    }
                }
            }),
        ]
    }

    fn has(&self, name: &str) -> bool {
        matches!(
            name,
            "computer_displays" | "computer_screenshot" | "computer_action"
        )
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<ToolOutput, String> {
        let obj = arguments
            .as_object()
            .cloned()
            .unwrap_or_default();

        match name {
            "computer_displays" => {
                let backend = self.backend.clone();
                let (permissions, displays) = tokio::task::spawn_blocking(move || {
                    let perms = backend.permissions();
                    let displays = backend.list_displays();
                    (perms, displays)
                })
                .await
                .map_err(|err| format!("display probe failed: {err}"))?;
                let displays = displays?;
                let value = json!({
                    "permissions": permissions,
                    "displays": displays,
                });
                let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                Ok(ToolOutput {
                    content: vec![json!({ "type": "text", "text": text })],
                    structured: Some(value),
                })
            }
            "computer_screenshot" => {
                let selector = parse_display_selector(obj.get("display"))?;
                let max_width = parse_dim(
                    obj.get("max_width"),
                    DEFAULT_MAX_WIDTH,
                    MAX_ALLOWED_WIDTH,
                    "max_width",
                )?;
                let max_height = parse_dim(
                    obj.get("max_height"),
                    DEFAULT_MAX_HEIGHT,
                    MAX_ALLOWED_HEIGHT,
                    "max_height",
                )?;

                let mut state = self.state.lock().await;
                let shot = self.capture(selector, max_width, max_height).await?;
                state.last_frame = Some(shot.geometry.clone());
                Ok(screenshot_tool_output(&shot, None))
            }
            "computer_action" => {
                let parsed_actions: Vec<ParsedAction> = if let Some(arr) = obj.get("actions") {
                    let items = arr
                        .as_array()
                        .ok_or_else(|| "`actions` must be an array of action objects".to_owned())?;
                    if items.is_empty() {
                        return Err("`actions` must not be empty".into());
                    }
                    if items.len() > MAX_BATCH_ACTIONS {
                        return Err(format!(
                            "`actions` exceeds maximum batch size of {MAX_BATCH_ACTIONS}"
                        ));
                    }
                    items
                        .iter()
                        .map(|item| {
                            let map = item.as_object().ok_or_else(|| {
                                "each entry in `actions` must be an object".to_owned()
                            })?;
                            parse_action_object(map)
                        })
                        .collect::<Result<_, _>>()?
                } else {
                    vec![parse_action_object(&obj)?]
                };

                let capture_after = obj
                    .get("capture_after")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let settle_ms = obj
                    .get("settle_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_SETTLE_MS)
                    .min(MAX_SETTLE_MS);

                let mut state = self.state.lock().await;
                let (resolved, explicit_screenshot) =
                    resolve_actions(&parsed_actions, state.last_frame.as_ref())?;
                let executed = resolved.len();

                if !resolved.is_empty() {
                    let backend = self.backend.clone();
                    tokio::task::spawn_blocking(move || {
                        for action in &resolved {
                            backend.execute_action(action)?;
                        }
                        Ok::<(), String>(())
                    })
                    .await
                    .map_err(|err| format!("computer action task failed: {err}"))??;
                }

                if capture_after || explicit_screenshot {
                    if executed > 0 && settle_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(settle_ms)).await;
                    }
                    let (selector, max_w, max_h) = state
                        .last_frame
                        .as_ref()
                        .map(|f| (f.selector.clone(), f.max_width, f.max_height))
                        .unwrap_or((
                            DisplaySelector::Primary,
                            DEFAULT_MAX_WIDTH,
                            DEFAULT_MAX_HEIGHT,
                        ));
                    let shot = self.capture(selector, max_w, max_h).await?;
                    state.last_frame = Some(shot.geometry.clone());
                    Ok(screenshot_tool_output(
                        &shot,
                        Some(json!({ "executedActions": executed })),
                    ))
                } else {
                    let value = json!({
                        "ok": true,
                        "executedActions": executed,
                    });
                    let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                    Ok(ToolOutput {
                        content: vec![json!({ "type": "text", "text": text })],
                        structured: Some(value),
                    })
                }
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

/// Serve the Crew desktop computer-use MCP server on stdin/stdout until stdin closes.
pub async fn run_stdio() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let backend = Arc::new(macos::MacOsBackend);
    #[cfg(not(target_os = "macos"))]
    let backend = Arc::new(UnsupportedBackend);

    let server = Arc::new(ComputerUseServer::new(backend));
    serve_stdio_handler(server).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::handle_request_with;
    use std::sync::Mutex as StdMutex;

    struct FakeBackend {
        permissions: PermissionStatus,
        displays: Vec<DisplayInfo>,
        executed: StdMutex<Vec<ResolvedAction>>,
    }

    impl FakeBackend {
        fn dual_monitor() -> Self {
            Self {
                permissions: PermissionStatus {
                    accessibility: true,
                    screen_recording: true,
                },
                displays: vec![
                    DisplayInfo {
                        id: 1,
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 1080,
                        is_primary: true,
                    },
                    DisplayInfo {
                        id: 2,
                        x: -1280,
                        y: 0,
                        width: 1280,
                        height: 720,
                        is_primary: false,
                    },
                ],
                executed: StdMutex::new(Vec::new()),
            }
        }
    }

    impl ComputerBackend for FakeBackend {
        fn permissions(&self) -> PermissionStatus {
            self.permissions
        }

        fn list_displays(&self) -> Result<Vec<DisplayInfo>, String> {
            Ok(self.displays.clone())
        }

        fn capture_display(&self, display: &DisplayInfo) -> Result<RgbaImage, String> {
            // Simulate 2x Retina capture at 2x logical dimensions (clamped small for fast tests).
            let w = (display.width / 2).max(16);
            let h = (display.height / 2).max(16);
            Ok(RgbaImage::from_pixel(w, h, image::Rgba([40, 80, 120, 255])))
        }

        fn execute_action(&self, action: &ResolvedAction) -> Result<(), String> {
            self.executed.lock().unwrap().push(action.clone());
            Ok(())
        }
    }

    #[test]
    fn keypress_parses_chords_and_compact_plus_syntax() {
        let (mods, key) = parse_keypress(&["CMD".into(), "SHIFT".into(), "P".into()]).unwrap();
        assert_eq!(mods, vec![Modifier::Command, Modifier::Shift]);
        assert_eq!(key, Some(KeyCodeSpec::Virtual(0x23)));

        let (mods2, key2) = parse_keypress(&["cmd+l".into()]).unwrap();
        assert_eq!(mods2, vec![Modifier::Command]);
        assert_eq!(key2, Some(KeyCodeSpec::Virtual(0x25)));

        let (mods3, key3) = parse_keypress(&["ENTER".into()]).unwrap();
        assert!(mods3.is_empty());
        assert_eq!(key3, Some(KeyCodeSpec::Virtual(0x24)));
    }

    #[tokio::test]
    async fn mcp_handshake_screenshot_and_action_coordinate_mapping() {
        let backend = Arc::new(FakeBackend::dual_monitor());
        let server = ComputerUseServer::new(backend.clone());

        let init = handle_request_with(
            &server,
            json!(1),
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        )
        .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "computer_use");

        let list = handle_request_with(&server, json!(2), "tools/list", Value::Null).await;
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["computer_displays", "computer_screenshot", "computer_action"]
        );

        // Coordinate action before screenshot must fail closed.
        let before = handle_request_with(
            &server,
            json!(3),
            "tools/call",
            json!({
                "name": "computer_action",
                "arguments": { "action": "click", "x": 100, "y": 100 }
            }),
        )
        .await;
        assert_eq!(before["result"]["isError"], true);

        // Capture primary display (1920x1080 -> scaled to 1280x720).
        let shot = handle_request_with(
            &server,
            json!(4),
            "tools/call",
            json!({
                "name": "computer_screenshot",
                "arguments": {}
            }),
        )
        .await;
        assert_eq!(shot["result"]["isError"], false);
        assert_eq!(shot["result"]["structuredContent"]["width"], 1280);
        assert_eq!(shot["result"]["structuredContent"]["height"], 720);
        let content = shot["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["mimeType"], "image/png");
        assert!(!content[1]["data"].as_str().unwrap().is_empty());

        // Click at center (640, 360) + type + keypress in one batch.
        let act = handle_request_with(
            &server,
            json!(5),
            "tools/call",
            json!({
                "name": "computer_action",
                "arguments": {
                    "actions": [
                        { "action": "click", "x": 640, "y": 360 },
                        { "action": "type", "text": "hello" },
                        { "action": "keypress", "keys": ["RETURN"] }
                    ],
                    "settle_ms": 0
                }
            }),
        )
        .await;
        assert_eq!(act["result"]["isError"], false);
        assert_eq!(act["result"]["structuredContent"]["executedActions"], 3);
        let executed = backend.executed.lock().unwrap();
        assert_eq!(executed.len(), 3);
        match &executed[0] {
            ResolvedAction::Click { x, y, button, clicks, .. } => {
                assert!((x - 960.75).abs() < 1.0, "expected ~960, got {x}");
                assert!((y - 540.75).abs() < 1.0, "expected ~540, got {y}");
                assert_eq!(*button, MouseButton::Left);
                assert_eq!(*clicks, 1);
            }
            other => panic!("expected Click, got {other:?}"),
        }
        assert_eq!(
            executed[1],
            ResolvedAction::Type {
                text: "hello".into()
            }
        );
        assert_eq!(
            executed[2],
            ResolvedAction::Keypress {
                modifiers: vec![],
                key: Some(KeyCodeSpec::Virtual(0x24)),
            }
        );
    }

    #[tokio::test]
    async fn multi_monitor_negative_origin_maps_accurately() {
        let backend = Arc::new(FakeBackend::dual_monitor());
        let server = ComputerUseServer::new(backend.clone());

        // Capture all displays: [-1280..1920] = 3200 wide, 1080 tall -> scaled to 1280x432 (scale = 0.4).
        let shot = handle_request_with(
            &server,
            json!(1),
            "tools/call",
            json!({
                "name": "computer_screenshot",
                "arguments": { "display": "all" }
            }),
        )
        .await;
        assert_eq!(shot["result"]["isError"], false);
        assert_eq!(shot["result"]["structuredContent"]["width"], 1280);
        assert_eq!(shot["result"]["structuredContent"]["height"], 432);

        // Pixel (0, 0) lies on the left secondary display (-1280, 0).
        let act = handle_request_with(
            &server,
            json!(2),
            "tools/call",
            json!({
                "name": "computer_action",
                "arguments": { "action": "move", "x": 0, "y": 0, "capture_after": false }
            }),
        )
        .await;
        assert_eq!(act["result"]["isError"], false);
        let executed = backend.executed.lock().unwrap();
        match &executed[0] {
            ResolvedAction::Move { x, y } => {
                assert!((x - (-1280.0)).abs() < 2.0, "expected ~-1280, got {x}");
                assert!((y - 0.0).abs() < 2.0, "expected ~0, got {y}");
            }
            other => panic!("expected Move, got {other:?}"),
        }

        // Pixel (100, 400) is below the 720*0.4=288px left display -> dead space rejected.
        let dead = handle_request_with(
            &server,
            json!(3),
            "tools/call",
            json!({
                "name": "computer_action",
                "arguments": { "action": "move", "x": 100, "y": 400, "capture_after": false }
            }),
        )
        .await;
        assert_eq!(dead["result"]["isError"], true);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "exercises real macOS CoreGraphics + screencapture permissions"]
    async fn macos_live_displays_and_screenshot_smoke() {
        let server = ComputerUseServer::new(Arc::new(macos::MacOsBackend));

        let init = handle_request_with(
            &server,
            json!(1),
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        )
        .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "computer_use");

        let list = handle_request_with(&server, json!(2), "tools/list", Value::Null).await;
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);

        let displays = handle_request_with(
            &server,
            json!(3),
            "tools/call",
            json!({ "name": "computer_displays", "arguments": {} }),
        )
        .await;
        assert_eq!(displays["result"]["isError"], false, "{displays}");
        let sc = &displays["result"]["structuredContent"];
        assert!(sc["displays"].as_array().is_some_and(|d| !d.is_empty()));
        println!("live computer_displays: {sc}");

        let shot = handle_request_with(
            &server,
            json!(4),
            "tools/call",
            json!({ "name": "computer_screenshot", "arguments": {} }),
        )
        .await;
        assert_eq!(shot["result"]["isError"], false, "{shot}");
        let shot_sc = &shot["result"]["structuredContent"];
        let width = shot_sc["width"].as_u64().unwrap();
        let height = shot_sc["height"].as_u64().unwrap();
        assert!(width > 0 && width <= DEFAULT_MAX_WIDTH as u64);
        assert!(height > 0 && height <= DEFAULT_MAX_HEIGHT as u64);
        let content = shot["result"]["content"].as_array().unwrap();
        let image_block = content
            .iter()
            .find(|item| item["type"] == "image")
            .expect("image content block");
        let b64 = image_block["data"].as_str().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("valid base64");
        let decoded = image::load_from_memory(&bytes).expect("valid encoded screenshot");
        assert_eq!(decoded.width() as u64, width);
        assert_eq!(decoded.height() as u64, height);
        println!(
            "live computer_screenshot: {}x{} ({}, {} bytes base64)",
            width,
            height,
            image_block["mimeType"].as_str().unwrap_or("?"),
            b64.len()
        );
    }
}
