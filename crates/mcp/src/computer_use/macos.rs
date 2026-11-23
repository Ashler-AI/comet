//! macOS CoreGraphics + `/usr/sbin/screencapture` backend for Crew computer use.
//!
//! Uses only Apple system frameworks (`CoreGraphics`, `CoreFoundation`,
//! `ApplicationServices`) and `/usr/sbin/screencapture`, so `Crew Engine` stays
//! headless with no AppKit/NSApplication dependency while inheriting the host
//! app bundle's macOS Accessibility and Screen Recording TCC grants.

use std::ffi::c_void;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use image::{DynamicImage, RgbaImage};

use super::{
    ComputerBackend, DisplayInfo, KeyCodeSpec, Modifier, MouseButton, PermissionStatus,
    ResolvedAction,
};

const MAX_DISPLAYS: u32 = 32;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
const UTF16_CHUNK_LEN: usize = 20;

const CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
const CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
const CG_EVENT_MOUSE_MOVED: u32 = 5;
const CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
const CG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;

const CG_MOUSE_BUTTON_LEFT: u32 = 0;
const CG_MOUSE_BUTTON_RIGHT: u32 = 1;
const CG_MOUSE_BUTTON_CENTER: u32 = 2;

const CG_MOUSE_EVENT_CLICK_STATE: u32 = 1;
const CG_SCROLL_EVENT_UNIT_PIXEL: u32 = 0;
const CG_HID_EVENT_TAP: u32 = 0;

const CG_FLAG_SHIFT: u64 = 0x0002_0000;
const CG_FLAG_CONTROL: u64 = 0x0004_0000;
const CG_FLAG_ALTERNATE: u64 = 0x0008_0000;
const CG_FLAG_COMMAND: u64 = 0x0010_0000;

const KVK_RETURN: u16 = 0x24;
const KVK_TAB: u16 = 0x30;
const KVK_COMMAND: u16 = 0x37;
const KVK_SHIFT: u16 = 0x38;
const KVK_OPTION: u16 = 0x3A;
const KVK_CONTROL: u16 = 0x3B;

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGMainDisplayID() -> u32;
    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut u32,
        display_count: *mut u32,
    ) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGEventCreateMouseEvent(
        source: *const c_void,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> *mut c_void;
    fn CGEventCreateKeyboardEvent(
        source: *const c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> *mut c_void;
    fn CGEventKeyboardSetUnicodeString(
        event: *mut c_void,
        string_length: usize,
        unicode_string: *const u16,
    );
    fn CGEventCreateScrollWheelEvent2(
        source: *const c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *mut c_void;
    fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
    fn CGEventSetFlags(event: *mut c_void, flags: u64);
    fn CGEventPost(tap: u32, event: *mut c_void);
}

struct EventGuard(*mut c_void);

impl EventGuard {
    fn new(ptr: *mut c_void) -> Result<Self, String> {
        if ptr.is_null() {
            Err("CoreGraphics failed to allocate input event".into())
        } else {
            Ok(Self(ptr))
        }
    }

    fn post(&self) {
        unsafe { CGEventPost(CG_HID_EVENT_TAP, self.0) };
    }
}

impl Drop for EventGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0.cast()) };
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct MacOsBackend;

impl ComputerBackend for MacOsBackend {
    fn permissions(&self) -> PermissionStatus {
        PermissionStatus {
            accessibility: unsafe { AXIsProcessTrusted() },
            screen_recording: unsafe { CGPreflightScreenCaptureAccess() },
        }
    }

    fn list_displays(&self) -> Result<Vec<DisplayInfo>, String> {
        let mut ids = [0u32; MAX_DISPLAYS as usize];
        let mut count: u32 = 0;
        let err = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &mut count) };
        if err != 0 {
            return Err(format!("CGGetActiveDisplayList failed with status {err}"));
        }
        let main_id = unsafe { CGMainDisplayID() };
        let mut displays = Vec::with_capacity(count as usize);
        for &id in &ids[..(count as usize).min(ids.len())] {
            let bounds = unsafe { CGDisplayBounds(id) };
            let width = bounds.size.width.round().max(1.0) as u32;
            let height = bounds.size.height.round().max(1.0) as u32;
            displays.push(DisplayInfo {
                id,
                x: bounds.origin.x.round() as i32,
                y: bounds.origin.y.round() as i32,
                width,
                height,
                is_primary: id == main_id,
            });
        }
        displays.sort_by_key(|d| (!d.is_primary, d.x, d.y, d.id));
        if displays.is_empty() {
            return Err("no active macOS displays found".into());
        }
        Ok(displays)
    }

    fn capture_display(&self, display: &DisplayInfo) -> Result<RgbaImage, String> {
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            return Err(
                "macOS Screen Recording permission is not granted for Crew. Enable it in System Settings → Privacy & Security → Screen Recording (or Crew Settings → Permissions) and restart Crew."
                    .into(),
            );
        }

        let file = tempfile::Builder::new()
            .prefix("crew-computer-use-")
            .suffix(".png")
            .tempfile()
            .map_err(|err| format!("failed to create temporary screenshot file: {err}"))?;

        let rect = format!(
            "-R{},{},{},{}",
            display.x, display.y, display.width, display.height
        );
        let mut child = Command::new("/usr/sbin/screencapture")
            .arg("-x")
            .arg(rect)
            .arg(file.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("failed to start /usr/sbin/screencapture: {err}"))?;

        let deadline = Instant::now() + CAPTURE_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("failed while waiting for screencapture: {err}"));
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("macOS screen capture exceeded its 5-second timeout".into());
            }
            thread::sleep(Duration::from_millis(10));
        };

        if !status.success() {
            if !unsafe { CGPreflightScreenCaptureAccess() } {
                return Err(
                    "macOS Screen Recording permission is not granted for Crew. Enable it in System Settings → Privacy & Security → Screen Recording and restart Crew."
                        .into(),
                );
            }
            return Err(format!("macOS screencapture exited with {status}"));
        }

        image::open(file.path())
            .map_err(|err| format!("failed to decode macOS screenshot PNG: {err}"))
            .map(DynamicImage::into_rgba8)
    }

    fn execute_action(&self, action: &ResolvedAction) -> Result<(), String> {
        if let ResolvedAction::Wait { duration_ms } = action {
            thread::sleep(Duration::from_millis(*duration_ms));
            return Ok(());
        }

        if !unsafe { AXIsProcessTrusted() } {
            return Err(
                "macOS Accessibility permission is not granted for Crew. Enable it in System Settings → Privacy & Security → Accessibility (or Crew Settings → Permissions)."
                    .into(),
            );
        }

        match action {
            ResolvedAction::Move { x, y } => {
                post_mouse(CG_EVENT_MOUSE_MOVED, CG_MOUSE_BUTTON_LEFT, *x, *y, 0, 0)
            }
            ResolvedAction::Click {
                x,
                y,
                button,
                clicks,
                modifiers,
            } => {
                let flags = modifier_flags(modifiers);
                let (down_type, up_type, _, cg_button) = mouse_button_codes(*button);
                post_mouse(CG_EVENT_MOUSE_MOVED, CG_MOUSE_BUTTON_LEFT, *x, *y, 0, flags)?;
                thread::sleep(Duration::from_millis(10));
                for click_state in 1..=*clicks {
                    post_mouse(down_type, cg_button, *x, *y, i64::from(click_state), flags)?;
                    thread::sleep(Duration::from_millis(15));
                    post_mouse(up_type, cg_button, *x, *y, i64::from(click_state), flags)?;
                    if click_state < *clicks {
                        thread::sleep(Duration::from_millis(30));
                    }
                }
                Ok(())
            }
            ResolvedAction::Drag {
                path,
                button,
                modifiers,
            } => {
                let flags = modifier_flags(modifiers);
                let (down_type, up_type, drag_type, cg_button) = mouse_button_codes(*button);
                let first = path[0];
                post_mouse(
                    CG_EVENT_MOUSE_MOVED,
                    CG_MOUSE_BUTTON_LEFT,
                    first.0,
                    first.1,
                    0,
                    flags,
                )?;
                thread::sleep(Duration::from_millis(10));
                post_mouse(down_type, cg_button, first.0, first.1, 1, flags)?;
                thread::sleep(Duration::from_millis(15));
                for pair in path.windows(2) {
                    let (x0, y0) = pair[0];
                    let (x1, y1) = pair[1];
                    let dist = ((x1 - x0).hypot(y1 - y0)).ceil() as usize;
                    let steps = (dist / 12).clamp(4, 32);
                    for step in 1..=steps {
                        let t = step as f64 / steps as f64;
                        let px = x0 + (x1 - x0) * t;
                        let py = y0 + (y1 - y0) * t;
                        post_mouse(drag_type, cg_button, px, py, 1, flags)?;
                        thread::sleep(Duration::from_millis(6));
                    }
                }
                let last = *path.last().expect("drag path validated non-empty");
                post_mouse(up_type, cg_button, last.0, last.1, 1, flags)
            }
            ResolvedAction::Scroll {
                x,
                y,
                scroll_x,
                scroll_y,
                modifiers,
            } => {
                let flags = modifier_flags(modifiers);
                post_mouse(CG_EVENT_MOUSE_MOVED, CG_MOUSE_BUTTON_LEFT, *x, *y, 0, flags)?;
                thread::sleep(Duration::from_millis(10));
                // Standard screen coordinates: positive scroll_y scrolls content down,
                // positive scroll_x scrolls content right. macOS Quartz wheel deltas
                // have the opposite sign in pixel mode.
                let wheel_y = scroll_y.saturating_neg();
                let wheel_x = scroll_x.saturating_neg();
                let ev = EventGuard::new(unsafe {
                    CGEventCreateScrollWheelEvent2(
                        std::ptr::null(),
                        CG_SCROLL_EVENT_UNIT_PIXEL,
                        2,
                        wheel_y,
                        wheel_x,
                        0,
                    )
                })?;
                if flags != 0 {
                    unsafe { CGEventSetFlags(ev.0, flags) };
                }
                ev.post();
                Ok(())
            }
            ResolvedAction::Type { text } => type_text(text),
            ResolvedAction::Keypress { modifiers, key } => keypress(modifiers, key.as_ref()),
            ResolvedAction::Wait { .. } => unreachable!("handled above"),
        }
    }
}

fn mouse_button_codes(button: MouseButton) -> (u32, u32, u32, u32) {
    match button {
        MouseButton::Left => (
            CG_EVENT_LEFT_MOUSE_DOWN,
            CG_EVENT_LEFT_MOUSE_UP,
            CG_EVENT_LEFT_MOUSE_DRAGGED,
            CG_MOUSE_BUTTON_LEFT,
        ),
        MouseButton::Right => (
            CG_EVENT_RIGHT_MOUSE_DOWN,
            CG_EVENT_RIGHT_MOUSE_UP,
            CG_EVENT_RIGHT_MOUSE_DRAGGED,
            CG_MOUSE_BUTTON_RIGHT,
        ),
        MouseButton::Middle => (
            CG_EVENT_OTHER_MOUSE_DOWN,
            CG_EVENT_OTHER_MOUSE_UP,
            CG_EVENT_OTHER_MOUSE_DRAGGED,
            CG_MOUSE_BUTTON_CENTER,
        ),
    }
}

fn modifier_flags(modifiers: &[Modifier]) -> u64 {
    let mut flags = 0u64;
    for modifier in modifiers {
        flags |= match modifier {
            Modifier::Shift => CG_FLAG_SHIFT,
            Modifier::Control => CG_FLAG_CONTROL,
            Modifier::Alt => CG_FLAG_ALTERNATE,
            Modifier::Command => CG_FLAG_COMMAND,
        };
    }
    flags
}

fn modifier_keycode(modifier: Modifier) -> u16 {
    match modifier {
        Modifier::Shift => KVK_SHIFT,
        Modifier::Control => KVK_CONTROL,
        Modifier::Alt => KVK_OPTION,
        Modifier::Command => KVK_COMMAND,
    }
}

fn post_mouse(
    event_type: u32,
    button: u32,
    x: f64,
    y: f64,
    click_state: i64,
    flags: u64,
) -> Result<(), String> {
    let ev = EventGuard::new(unsafe {
        CGEventCreateMouseEvent(std::ptr::null(), event_type, CGPoint { x, y }, button)
    })?;
    if click_state > 0 {
        unsafe { CGEventSetIntegerValueField(ev.0, CG_MOUSE_EVENT_CLICK_STATE, click_state) };
    }
    if flags != 0 {
        unsafe { CGEventSetFlags(ev.0, flags) };
    }
    ev.post();
    Ok(())
}

fn post_key(keycode: u16, key_down: bool, flags: u64) -> Result<(), String> {
    let ev = EventGuard::new(unsafe {
        CGEventCreateKeyboardEvent(std::ptr::null(), keycode, key_down)
    })?;
    if flags != 0 {
        unsafe { CGEventSetFlags(ev.0, flags) };
    }
    ev.post();
    Ok(())
}

fn post_unicode_slice(units: &[u16], flags: u64) -> Result<(), String> {
    let down = EventGuard::new(unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), 0, true) })?;
    let up = EventGuard::new(unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), 0, false) })?;
    unsafe {
        if flags != 0 {
            CGEventSetFlags(down.0, flags);
            CGEventSetFlags(up.0, flags);
        }
        CGEventKeyboardSetUnicodeString(down.0, units.len(), units.as_ptr());
        CGEventKeyboardSetUnicodeString(up.0, units.len(), units.as_ptr());
    }
    down.post();
    thread::sleep(Duration::from_millis(6));
    up.post();
    Ok(())
}

fn type_text(text: &str) -> Result<(), String> {
    let mut buffer = Vec::with_capacity(UTF16_CHUNK_LEN);
    let flush = |buf: &mut Vec<u16>| -> Result<(), String> {
        if buf.is_empty() {
            return Ok(());
        }
        post_unicode_slice(buf, 0)?;
        buf.clear();
        thread::sleep(Duration::from_millis(6));
        Ok(())
    };

    for ch in text.chars() {
        match ch {
            '\r' => {}
            '\n' => {
                flush(&mut buffer)?;
                post_key(KVK_RETURN, true, 0)?;
                thread::sleep(Duration::from_millis(8));
                post_key(KVK_RETURN, false, 0)?;
                thread::sleep(Duration::from_millis(8));
            }
            '\t' => {
                flush(&mut buffer)?;
                post_key(KVK_TAB, true, 0)?;
                thread::sleep(Duration::from_millis(8));
                post_key(KVK_TAB, false, 0)?;
                thread::sleep(Duration::from_millis(8));
            }
            other => {
                let mut encoded = [0u16; 2];
                let slice = other.encode_utf16(&mut encoded);
                if buffer.len() + slice.len() > UTF16_CHUNK_LEN {
                    flush(&mut buffer)?;
                }
                buffer.extend_from_slice(slice);
            }
        }
    }
    flush(&mut buffer)
}

fn keypress(modifiers: &[Modifier], key: Option<&KeyCodeSpec>) -> Result<(), String> {
    let flags = modifier_flags(modifiers);
    for &modifier in modifiers {
        post_key(modifier_keycode(modifier), true, flags)?;
        thread::sleep(Duration::from_millis(6));
    }
    match key {
        Some(KeyCodeSpec::Virtual(code)) => {
            post_key(*code, true, flags)?;
            thread::sleep(Duration::from_millis(12));
            post_key(*code, false, flags)?;
            thread::sleep(Duration::from_millis(6));
        }
        Some(KeyCodeSpec::Unicode(ch)) => {
            let mut encoded = [0u16; 2];
            let slice = ch.encode_utf16(&mut encoded);
            post_unicode_slice(slice, flags)?;
            thread::sleep(Duration::from_millis(6));
        }
        None => {
            thread::sleep(Duration::from_millis(12));
        }
    }
    for &modifier in modifiers.iter().rev() {
        post_key(modifier_keycode(modifier), false, 0)?;
        thread::sleep(Duration::from_millis(6));
    }
    Ok(())
}
