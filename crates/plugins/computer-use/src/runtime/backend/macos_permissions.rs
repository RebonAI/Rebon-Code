//! The two macOS privacy (TCC) grants desktop control needs: Screen Recording
//! and Accessibility.
//!
//! These are the only place the process asks macOS about them. The backend
//! reads them before every action, and the desktop app's onboarding reads and
//! requests them so the user can switch them on before Computer Use first runs.
//! Both answer for the calling process — in the app that is Rebon.app itself.

use cocoa::base::{id, nil};
use cocoa::foundation::{NSAutoreleasePool, NSString};
use core_foundation::dictionary::CFDictionaryRef;
use core_graphics::access::ScreenCaptureAccess;
use objc::{class, msg_send, sel, sel_impl};

use crate::runtime::{PermissionState, PermissionStatus};

/// Current grants, without prompting.
pub fn status() -> PermissionStatus {
    PermissionStatus {
        screen_recording: grant(screen_recording_granted()),
        accessibility: grant(accessibility_granted()),
    }
}

pub fn screen_recording_granted() -> bool {
    ScreenCaptureAccess.preflight()
}

pub fn accessibility_granted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Adds this app to System Settings › Privacy & Security › Screen & System
/// Audio Recording. macOS shows its own alert only the first time; later
/// calls return without one.
pub fn request_screen_recording() {
    let _ = ScreenCaptureAccess.request();
}

/// Adds this app to System Settings › Privacy & Security › Accessibility and
/// shows macOS's alert pointing there while the grant is off.
pub fn request_accessibility() {
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let key = NSString::alloc(nil).init_str("AXTrustedCheckOptionPrompt");
        let yes: id = msg_send![class!(NSNumber), numberWithBool: true];
        let options: id = msg_send![class!(NSDictionary), dictionaryWithObject: yes forKey: key];
        let _ = AXIsProcessTrustedWithOptions(options as CFDictionaryRef);
        let _: () = msg_send![key, release];
        let _: () = msg_send![pool, drain];
    }
}

fn grant(granted: bool) -> PermissionState {
    if granted {
        PermissionState::Granted
    } else {
        PermissionState::Denied
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
}
