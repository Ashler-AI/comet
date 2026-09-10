//! Process-local TCC checks and explicit requests. Never used by the daemon.

use std::ffi::{c_int, c_void};
use std::ptr;

type CFTypeRef = *const c_void;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    // Accessibility returns Core Foundation Boolean (UInt8), not C bool.
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> u8;
    static kAXTrustedCheckOptionPrompt: CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFTypeRef;
    fn CFRelease(value: CFTypeRef);
}

unsafe extern "C" {
    fn pthread_main_np() -> c_int;
}

/// Nonprompting checks. CoreGraphics may contact TCC, so run off the UI thread.
pub(super) fn status() -> [bool; 2] {
    // SAFETY: These parameterless APIs report access for this process only.
    unsafe { [AXIsProcessTrusted() != 0, CGPreflightScreenCaptureAccess()] }
}

/// Called only by a GPUI click handler on the application main thread. The AX
/// prompt is asynchronous; the return value is not the user's eventual answer.
pub(super) fn request_accessibility() -> Result<(), &'static str> {
    // SAFETY: pthread_main_np is parameterless and valid on every macOS thread.
    if unsafe { pthread_main_np() } == 0 {
        return Err("Request Accessibility from the Crew desktop window.");
    }
    // SAFETY: Both dictionary entries are immortal CF objects. Null callbacks
    // mean the dictionary borrows them; the stack arrays are copied at creation.
    // The created dictionary is released exactly once after the synchronous call.
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            ptr::null(),
            ptr::null(),
        );
        if options.is_null() {
            return Err(
                "Could not prepare the Accessibility request. Open System Settings instead.",
            );
        }
        AXIsProcessTrustedWithOptions(options);
        CFRelease(options);
    }
    Ok(())
}

/// CoreGraphics' synchronous request can wait for the system dialog. It has no
/// AppKit main-thread requirement; keep it off GPUI's event loop. Always follow
/// it with preflight: a prompt result is not proof this process can capture yet.
pub(super) fn request_screen_recording() {
    // SAFETY: Parameterless CoreGraphics API, reached only after an explicit click.
    unsafe {
        CGRequestScreenCaptureAccess();
    }
}
