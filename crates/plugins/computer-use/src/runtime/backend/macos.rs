use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use cocoa::base::{id, nil};
use cocoa::foundation::{NSAutoreleasePool, NSString};
use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionaryRef;
use core_graphics::access::ScreenCaptureAccess;
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_graphics::display::{
    kCGWindowImageBestResolution, kCGWindowImageBoundsIgnoreFraming,
    kCGWindowListExcludeDesktopElements, kCGWindowListOptionIncludingWindow,
    kCGWindowListOptionOnScreenOnly, CGDisplay,
};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField, KeyCode,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::image::{CGImageAlphaInfo, CGImageByteOrderInfo};
use foreign_types::ForeignType;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder, RgbaImage};
use objc::{class, msg_send, sel, sel_impl};

use super::overlay::Overlay;
use super::Backend;
use crate::runtime::{
    Action, ActionResponse, ComputerUseError, ErrorCode, KeyModifier, MouseButton, PermissionState,
    PermissionStatus, Point, Rect, ScreenshotResponse, ServiceState, StatusResponse, TargetWindow,
    ACTIVE_PATH_ENV, MAX_TYPE_CHARS, MAX_WAIT_MS,
};

use super::common::{
    action_cancelled, bounded_dimensions, capture_dimensions_error, capture_frame_matches,
    invalid_input, marker_error,
};

pub struct MacBackend {
    target: Option<WindowRecord>,
    overlay: Option<Overlay>,
    state: ServiceState,
    target_epoch: u64,
    coordinate_scale_x: f64,
    coordinate_scale_y: f64,
    last_capture_frame: Option<Rect>,
    active_marker: Option<PathBuf>,
    cancel_requested: Arc<AtomicBool>,
}

impl MacBackend {
    pub fn new() -> Result<Self, ComputerUseError> {
        Self::new_with_cancel_signal(Arc::new(AtomicBool::new(false)))
    }

    pub fn new_with_cancel_signal(
        cancel_requested: Arc<AtomicBool>,
    ) -> Result<Self, ComputerUseError> {
        Ok(Self {
            target: None,
            overlay: None,
            state: ServiceState::WaitingForTarget,
            target_epoch: 0,
            coordinate_scale_x: 1.0,
            coordinate_scale_y: 1.0,
            last_capture_frame: None,
            active_marker: std::env::var_os(ACTIVE_PATH_ENV).map(PathBuf::from),
            cancel_requested,
        })
    }

    pub fn reset_for_selection(&mut self) -> Result<(), ComputerUseError> {
        self.clear_active_marker();
        if let Some(mut overlay) = self.overlay.take() {
            overlay.stop();
        }
        self.target = None;
        self.target_epoch = self.target_epoch.wrapping_add(1).max(1);
        self.coordinate_scale_x = 1.0;
        self.coordinate_scale_y = 1.0;
        self.last_capture_frame = None;
        self.state = ServiceState::WaitingForTarget;
        self.cancel_requested.store(false, Ordering::Release);
        Ok(())
    }

    /// Returns ordinary, visible application windows in front-to-back order.
    pub fn windows(&self) -> Result<Vec<TargetWindow>, ComputerUseError> {
        Ok(enumerate_windows()?
            .into_iter()
            .map(|window| window.public())
            .collect())
    }

    fn permissions(&self) -> PermissionStatus {
        PermissionStatus {
            screen_recording: if ScreenCaptureAccess.preflight() {
                PermissionState::Granted
            } else {
                PermissionState::Denied
            },
            accessibility: if unsafe { AXIsProcessTrusted() } {
                PermissionState::Granted
            } else {
                PermissionState::Denied
            },
        }
    }

    fn clear_active_marker(&self) {
        if let Some(path) = self.active_marker.as_ref() {
            let _ = std::fs::remove_file(path);
        }
    }

    fn mark_active(&self) -> Result<(), ComputerUseError> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        if self.cancel_requested.load(Ordering::Acquire) {
            return Err(action_cancelled());
        }
        let Some(path) = self.active_marker.as_ref() else {
            return Ok(());
        };
        match std::fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_type().is_file()
                    && metadata.permissions().mode() & 0o777 == 0o600 =>
            {
                if self.cancel_requested.load(Ordering::Acquire) {
                    self.clear_active_marker();
                    return Err(action_cancelled());
                }
                return Ok(());
            }
            Ok(_) => {
                return Err(ComputerUseError::new(
                    ErrorCode::Internal,
                    "Computer Use activation marker is not a private regular file",
                    false,
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(marker_error(error)),
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(marker_error)?;
        if self.cancel_requested.load(Ordering::Acquire) {
            self.clear_active_marker();
            return Err(action_cancelled());
        }
        Ok(())
    }

    fn pause_for_safety(&mut self) {
        if self.target.is_some() {
            self.state = ServiceState::Paused;
            self.clear_active_marker();
            if let Some(overlay) = self.overlay.as_mut() {
                overlay.dim();
            }
        }
    }

    fn require_capture_access(&mut self) -> Result<(), ComputerUseError> {
        if self.permissions().screen_recording != PermissionState::Granted {
            self.pause_for_safety();
            return Err(ComputerUseError::new(
                ErrorCode::PermissionDenied,
                "Screen Recording permission is required before Computer Use can act",
                false,
            ));
        }
        Ok(())
    }

    fn require_accessibility(&mut self) -> Result<(), ComputerUseError> {
        if self.permissions().accessibility != PermissionState::Granted {
            self.pause_for_safety();
            return Err(ComputerUseError::new(
                ErrorCode::PermissionDenied,
                "Accessibility permission is required for input",
                false,
            ));
        }
        Ok(())
    }

    fn acquire_target(&mut self, point: Point) -> Result<(), ComputerUseError> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(ComputerUseError::invalid_coordinates());
        }
        if self.target.is_some() {
            // Target identity is immutable for a backend session.
            return Ok(());
        }
        let target = enumerate_windows()?
            .into_iter()
            .find(|window| window.frame.contains_screen_point(point))
            .ok_or_else(|| {
                ComputerUseError::new(
                    ErrorCode::TargetNotSelected,
                    "no eligible window contains the requested screen point",
                    true,
                )
            })?;
        self.overlay = Some(Overlay::new(target.frame)?);
        self.target_epoch = self.target_epoch.wrapping_add(1).max(1);
        self.target = Some(target);
        self.state = ServiceState::Active;
        Ok(())
    }

    fn refresh_target(&mut self) -> Result<WindowRecord, ComputerUseError> {
        let locked = self.target.clone().ok_or_else(|| {
            ComputerUseError::new(
                ErrorCode::TargetNotSelected,
                "select a target with observe.target before using Computer Use",
                true,
            )
        })?;
        let current = enumerate_windows()?
            .into_iter()
            .find(|window| window.id == locked.id && window.owner_pid == locked.owner_pid);
        let Some(current) = current else {
            self.clear_active_marker();
            self.target = None;
            self.target_epoch = self.target_epoch.wrapping_add(1).max(1);
            if let Some(mut overlay) = self.overlay.take() {
                overlay.stop();
            }
            self.state = ServiceState::WaitingForTarget;
            return Err(ComputerUseError::new(
                ErrorCode::TargetInvalid,
                "the locked target window no longer exists or is no longer eligible",
                false,
            ));
        };
        if current.frame != locked.frame {
            if let Some(overlay) = self.overlay.as_mut() {
                overlay.follow(current.frame);
            }
        }
        self.target = Some(current.clone());
        Ok(current)
    }

    fn current_status(&self) -> StatusResponse {
        StatusResponse {
            state: self.state,
            permissions: self.permissions(),
            target_epoch: self.target_epoch,
            target: self.target.as_ref().map(WindowRecord::public),
        }
    }

    fn capture(&mut self) -> Result<ScreenshotResponse, ComputerUseError> {
        self.require_capture_access()?;
        let target = self.refresh_target()?;
        let bounds = cg_rect(target.frame);
        let image = CGDisplay::screenshot(
            bounds,
            kCGWindowListOptionIncludingWindow,
            target.id,
            kCGWindowImageBoundsIgnoreFraming | kCGWindowImageBestResolution,
        )
        .ok_or_else(|| {
            ComputerUseError::new(
                ErrorCode::CaptureFailed,
                "CoreGraphics did not return a target-window image",
                true,
            )
        })?;
        let width = u32::try_from(image.width()).map_err(|_| capture_dimensions_error())?;
        let height = u32::try_from(image.height()).map_err(|_| capture_dimensions_error())?;
        if width == 0 || height == 0 {
            return Err(capture_dimensions_error());
        }

        let color_space = CGColorSpace::create_device_rgb();
        let bytes_per_row = usize::try_from(width).unwrap() * 4;
        let bitmap_info = CGImageAlphaInfo::CGImageAlphaPremultipliedLast as u32
            | CGImageByteOrderInfo::CGImageByteOrder32Big as u32;
        let mut context = CGContext::create_bitmap_context(
            None,
            width as usize,
            height as usize,
            8,
            bytes_per_row,
            &color_space,
            bitmap_info,
        );
        context.translate(0.0, height as f64);
        context.scale(1.0, -1.0);
        context.draw_image(
            CGRect::new(
                &CGPoint::new(0.0, 0.0),
                &CGSize::new(width as f64, height as f64),
            ),
            &image,
        );
        let pixels = context.data().to_vec();
        let (output_width, output_height) = bounded_dimensions(width, height);
        let pixels = if (output_width, output_height) == (width, height) {
            pixels
        } else {
            let source =
                RgbaImage::from_raw(width, height, pixels).ok_or_else(capture_dimensions_error)?;
            image::imageops::resize(&source, output_width, output_height, FilterType::Triangle)
                .into_raw()
        };
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(
                &pixels,
                output_width,
                output_height,
                ExtendedColorType::Rgba8,
            )
            .map_err(|error| {
                ComputerUseError::new(
                    ErrorCode::CaptureFailed,
                    format!("failed to encode target screenshot: {error}"),
                    true,
                )
            })?;
        self.coordinate_scale_x = (output_width as f64 / target.frame.width).max(f64::EPSILON);
        self.coordinate_scale_y = (output_height as f64 / target.frame.height).max(f64::EPSILON);
        self.last_capture_frame = Some(target.frame);
        Ok(ScreenshotResponse {
            png_base64: BASE64.encode(png),
            width: output_width,
            height: output_height,
            scale: self.coordinate_scale_x,
        })
    }

    fn input_coordinates(&self, x: f64, y: f64) -> Result<(f64, f64), ComputerUseError> {
        if !self.coordinate_scale_x.is_finite()
            || !self.coordinate_scale_y.is_finite()
            || self.coordinate_scale_x <= 0.0
            || self.coordinate_scale_y <= 0.0
        {
            return Err(ComputerUseError::invalid_coordinates());
        }
        Ok((x / self.coordinate_scale_x, y / self.coordinate_scale_y))
    }

    fn input_target(&mut self) -> Result<WindowRecord, ComputerUseError> {
        self.require_accessibility()?;
        let target = self.refresh_target()?;
        if self.last_capture_frame.is_none() {
            return Err(ComputerUseError::new(
                ErrorCode::TargetInvalid,
                "observe the target before sending input",
                true,
            ));
        }
        if !capture_frame_matches(self.last_capture_frame, target.frame) {
            return Err(ComputerUseError::new(
                ErrorCode::TargetInvalid,
                "the target moved or resized; observe it again before sending input",
                true,
            ));
        }
        Ok(target)
    }

    fn wait_with_target_refresh(&mut self, duration: Duration) -> Result<(), ComputerUseError> {
        let deadline = Instant::now() + duration;
        let mut next_refresh = Instant::now();
        while Instant::now() < deadline {
            if self.cancel_requested.load(Ordering::Acquire) {
                return Err(action_cancelled());
            }
            if Instant::now() >= next_refresh {
                self.refresh_target()?;
                next_refresh = Instant::now() + Duration::from_millis(100);
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(25)),
            );
        }
        Ok(())
    }

    fn wait_unless_cancelled(&self, duration: Duration) -> Result<(), ComputerUseError> {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            if self.cancel_requested.load(Ordering::Acquire) {
                return Err(action_cancelled());
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(25)),
            );
        }
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<(), ComputerUseError> {
        if text.is_empty() {
            return Err(invalid_input("text input must not be empty"));
        }
        for chunk in text.encode_utf16().collect::<Vec<_>>().chunks(20) {
            ensure_not_cancelled(&self.cancel_requested)?;
            let target = self.input_target()?;
            ensure_target_frontmost_for_keyboard(&target)?;
            type_text_chunk(&target, chunk)?;
        }
        Ok(())
    }

    fn settle_after_input(&self) -> Result<(), ComputerUseError> {
        self.wait_unless_cancelled(Duration::from_millis(100))
    }
}

impl Backend for MacBackend {
    fn status(&mut self) -> Result<StatusResponse, ComputerUseError> {
        if self.state == ServiceState::Stopped {
            return Ok(self.current_status());
        }
        if self.target.is_some() {
            let _ = self.refresh_target();
            let permissions = self.permissions();
            if self.target.is_some()
                && (permissions.screen_recording != PermissionState::Granted
                    || permissions.accessibility != PermissionState::Granted)
            {
                self.pause_for_safety();
            }
        }
        Ok(self.current_status())
    }

    fn execute(&mut self, action: Action) -> Result<ActionResponse, ComputerUseError> {
        if self.cancel_requested.load(Ordering::Acquire) {
            return Err(ComputerUseError::new(
                ErrorCode::TargetInvalid,
                "Computer Use action was cancelled",
                true,
            ));
        }
        if self.state == ServiceState::Stopped {
            return Err(ComputerUseError::new(
                ErrorCode::TargetInvalid,
                "Computer Use runtime has stopped",
                false,
            ));
        }
        if self.state == ServiceState::Paused {
            return Err(ComputerUseError::new(
                ErrorCode::InvalidRequest,
                "Computer Use runtime is paused",
                true,
            ));
        }

        let is_observe = matches!(action, Action::Observe { .. });
        let screenshot = match action {
            Action::Observe { target } => {
                if let Some(point) = target {
                    self.acquire_target(point)?;
                }
                self.capture()?
            }
            Action::Click { x, y, button } => {
                self.require_capture_access()?;
                let (x, y) = self.input_coordinates(x, y)?;
                let target = self.input_target()?;
                click(&target, x, y, button, false, &self.cancel_requested)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::DoubleClick { x, y, button } => {
                self.require_capture_access()?;
                let (x, y) = self.input_coordinates(x, y)?;
                let target = self.input_target()?;
                click(&target, x, y, button, true, &self.cancel_requested)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::Move { x, y } => {
                self.require_capture_access()?;
                let (x, y) = self.input_coordinates(x, y)?;
                let target = self.input_target()?;
                move_pointer(&target, x, y, &self.cancel_requested)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::Scroll {
                x,
                y,
                delta_x,
                delta_y,
            } => {
                self.require_capture_access()?;
                let (x, y) = self.input_coordinates(x, y)?;
                let target = self.input_target()?;
                scroll(&target, x, y, delta_x, delta_y, &self.cancel_requested)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::Type { text } => {
                if text.chars().count() > MAX_TYPE_CHARS {
                    return Err(invalid_input("text input is too long"));
                }
                self.require_capture_access()?;
                self.type_text(&text)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::Key { key, modifiers } => {
                self.require_capture_access()?;
                let target = self.input_target()?;
                key_press(&target, &key, &modifiers, &self.cancel_requested)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::Wait { duration_ms } => {
                if duration_ms > MAX_WAIT_MS {
                    return Err(invalid_input("wait duration is too long"));
                }
                self.require_capture_access()?;
                self.refresh_target()?;
                self.state = ServiceState::Paused;
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.dim();
                }
                self.wait_with_target_refresh(Duration::from_millis(duration_ms))?;
                self.state = ServiceState::Active;
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.normal();
                }
                self.capture()?
            }
        };
        if self.cancel_requested.load(Ordering::Acquire) {
            return Err(ComputerUseError::new(
                ErrorCode::TargetInvalid,
                "Computer Use action was cancelled",
                true,
            ));
        }
        if !is_observe {
            if let Some(overlay) = self.overlay.as_mut() {
                overlay.pulse();
            }
        }
        self.mark_active()?;
        Ok(ActionResponse {
            status: self.current_status(),
            screenshot: Some(screenshot),
        })
    }

    fn request_permissions(&mut self) -> Result<StatusResponse, ComputerUseError> {
        let _ = ScreenCaptureAccess.request();
        unsafe {
            let pool = NSAutoreleasePool::new(nil);
            let key = NSString::alloc(nil).init_str("AXTrustedCheckOptionPrompt");
            let yes: id = msg_send![class!(NSNumber), numberWithBool: true];
            let options: id =
                msg_send![class!(NSDictionary), dictionaryWithObject: yes forKey: key];
            let _ = AXIsProcessTrustedWithOptions(options as CFDictionaryRef);
            let _: () = msg_send![key, release];
            let _: () = msg_send![pool, drain];
        }
        Ok(self.current_status())
    }

    fn pause(&mut self) -> Result<(), ComputerUseError> {
        if self.state == ServiceState::Stopped {
            return Err(invalid_input("runtime has stopped"));
        }
        self.state = ServiceState::Paused;
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.dim();
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<(), ComputerUseError> {
        if self.state != ServiceState::Paused {
            return Err(invalid_input("runtime is not paused"));
        }
        self.state = if self.target.is_some() {
            ServiceState::Active
        } else {
            ServiceState::WaitingForTarget
        };
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.normal();
        }
        if self.target.is_some() {
            self.mark_active()?;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), ComputerUseError> {
        self.clear_active_marker();
        if let Some(mut overlay) = self.overlay.take() {
            overlay.stop();
        }
        self.target = None;
        self.target_epoch = self.target_epoch.wrapping_add(1).max(1);
        self.state = ServiceState::Stopped;
        Ok(())
    }
}

impl Drop for MacBackend {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Clone, Debug)]
struct WindowRecord {
    id: u32,
    owner_pid: u32,
    owner_name: String,
    title: Option<String>,
    frame: Rect,
}

impl WindowRecord {
    fn public(&self) -> TargetWindow {
        TargetWindow {
            id: u64::from(self.id),
            owner_pid: self.owner_pid,
            owner_name: self.owner_name.clone(),
            title: self.title.clone(),
            frame: self.frame,
        }
    }
}

fn enumerate_windows() -> Result<Vec<WindowRecord>, ComputerUseError> {
    enumerate_windows_filtered(false, false)
}

fn enumerate_windows_for_input() -> Result<Vec<WindowRecord>, ComputerUseError> {
    enumerate_windows_filtered(true, true)
}

fn enumerate_windows_filtered(
    include_rebon: bool,
    include_system_windows: bool,
) -> Result<Vec<WindowRecord>, ComputerUseError> {
    let options = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
    let array = CGDisplay::window_list_info(options, None).ok_or_else(|| {
        ComputerUseError::new(
            ErrorCode::PermissionDenied,
            "unable to enumerate windows; Screen Recording permission may be missing",
            true,
        )
    })?;
    let self_pid = std::process::id();
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let array_id = array.as_concrete_TypeRef() as id;
        let count: usize = msg_send![array_id, count];
        let mut windows = Vec::new();
        for index in 0..count {
            let dictionary: id = msg_send![array_id, objectAtIndex: index];
            let id = number_u32(dictionary, "kCGWindowNumber").unwrap_or(0);
            let owner_pid = number_u32(dictionary, "kCGWindowOwnerPID").unwrap_or(0);
            let layer = number_i64(dictionary, "kCGWindowLayer").unwrap_or(-1);
            let alpha = number_f64(dictionary, "kCGWindowAlpha").unwrap_or(0.0);
            let onscreen = number_bool(dictionary, "kCGWindowIsOnscreen").unwrap_or(false);
            let owner_name = string_value(dictionary, "kCGWindowOwnerName").unwrap_or_default();
            let title = string_value(dictionary, "kCGWindowName").filter(|value| !value.is_empty());
            let bounds: id = dictionary_value(dictionary, "kCGWindowBounds");
            let frame = Rect {
                x: number_f64(bounds, "X").unwrap_or(f64::NAN),
                y: number_f64(bounds, "Y").unwrap_or(f64::NAN),
                width: number_f64(bounds, "Width").unwrap_or(0.0),
                height: number_f64(bounds, "Height").unwrap_or(0.0),
            };
            if id == 0
                || owner_pid == 0
                || (!include_rebon && owner_pid == self_pid)
                || layer != 0
                || alpha <= 0.01
                || !onscreen
                || (!include_system_windows && !eligible_owner(&owner_name))
                || !frame.is_valid()
                || frame.width < 32.0
                || frame.height < 32.0
            {
                continue;
            }
            windows.push(WindowRecord {
                id,
                owner_pid,
                owner_name,
                title,
                frame,
            });
        }
        let _: () = msg_send![pool, drain];
        Ok(windows)
    }
}

fn eligible_owner(owner: &str) -> bool {
    !matches!(
        owner,
        "Dock"
            | "Window Server"
            | "SystemUIServer"
            | "Control Center"
            | "Notification Center"
            | "loginwindow"
            | "Spotlight"
    )
}

unsafe fn dictionary_value(dictionary: id, key: &str) -> id {
    if dictionary == nil {
        return nil;
    }
    let key = NSString::alloc(nil).init_str(key);
    let value: id = msg_send![dictionary, objectForKey: key];
    let _: () = msg_send![key, release];
    value
}

unsafe fn number_u32(dictionary: id, key: &str) -> Option<u32> {
    let value = dictionary_value(dictionary, key);
    (value != nil).then(|| msg_send![value, unsignedIntValue])
}

unsafe fn number_i64(dictionary: id, key: &str) -> Option<i64> {
    let value = dictionary_value(dictionary, key);
    (value != nil).then(|| msg_send![value, longLongValue])
}

unsafe fn number_f64(dictionary: id, key: &str) -> Option<f64> {
    let value = dictionary_value(dictionary, key);
    (value != nil).then(|| msg_send![value, doubleValue])
}

unsafe fn number_bool(dictionary: id, key: &str) -> Option<bool> {
    let value = dictionary_value(dictionary, key);
    (value != nil).then(|| {
        let result: bool = msg_send![value, boolValue];
        result
    })
}

unsafe fn string_value(dictionary: id, key: &str) -> Option<String> {
    let value = dictionary_value(dictionary, key);
    if value == nil {
        return None;
    }
    let pointer: *const c_char = msg_send![value, UTF8String];
    if pointer.is_null() {
        None
    } else {
        Some(CStr::from_ptr(pointer).to_string_lossy().into_owned())
    }
}

fn cg_rect(frame: Rect) -> CGRect {
    CGRect::new(
        &CGPoint::new(frame.x, frame.y),
        &CGSize::new(frame.width, frame.height),
    )
}

fn source() -> Result<CGEventSource, ComputerUseError> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| input_error())
}

fn same_window(left: &WindowRecord, right: &WindowRecord) -> bool {
    left.id == right.id && left.owner_pid == right.owner_pid
}

fn target_is_frontmost_at_point(
    windows: &[WindowRecord],
    target: &WindowRecord,
    point: Point,
) -> bool {
    windows
        .iter()
        .find(|window| window.frame.contains_screen_point(point))
        .is_some_and(|window| same_window(window, target))
}

fn target_is_frontmost(windows: &[WindowRecord], target: &WindowRecord) -> bool {
    windows
        .first()
        .is_some_and(|window| same_window(window, target))
}

fn ensure_target_frontmost_at_point(
    target: &WindowRecord,
    point: Point,
) -> Result<(), ComputerUseError> {
    let windows = enumerate_windows_for_input()?;
    if target_is_frontmost_at_point(&windows, target, point) {
        Ok(())
    } else {
        Err(ComputerUseError::new(
            ErrorCode::TargetInvalid,
            "the locked window is not frontmost at the requested point",
            true,
        ))
    }
}

fn ensure_target_frontmost_for_keyboard(target: &WindowRecord) -> Result<(), ComputerUseError> {
    let windows = enumerate_windows_for_input()?;
    if target_is_frontmost(&windows, target) {
        Ok(())
    } else {
        Err(ComputerUseError::new(
            ErrorCode::TargetInvalid,
            "the locked window is not the frontmost window for keyboard input",
            true,
        ))
    }
}

fn ensure_not_cancelled(cancel_requested: &AtomicBool) -> Result<(), ComputerUseError> {
    if cancel_requested.load(Ordering::Acquire) {
        Err(action_cancelled())
    } else {
        Ok(())
    }
}

fn sleep_unless_cancelled(
    cancel_requested: &AtomicBool,
    duration: Duration,
) -> Result<(), ComputerUseError> {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        ensure_not_cancelled(cancel_requested)?;
        std::thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(10)),
        );
    }
    Ok(())
}

fn click(
    target: &WindowRecord,
    x: f64,
    y: f64,
    button: MouseButton,
    double: bool,
    cancel_requested: &AtomicBool,
) -> Result<(), ComputerUseError> {
    let point = target.frame.target_point(x, y)?;
    ensure_target_frontmost_at_point(target, point)?;
    let (button, down, up) = mouse_types(button);
    let repetitions = if double { 2 } else { 1 };
    for index in 0..repetitions {
        ensure_not_cancelled(cancel_requested)?;
        ensure_target_frontmost_at_point(target, point)?;
        let down_event = CGEvent::new_mouse_event(source()?, down, cg_point(point), button)
            .map_err(|_| input_error())?;
        let up_event = CGEvent::new_mouse_event(source()?, up, cg_point(point), button)
            .map_err(|_| input_error())?;
        if double {
            let click_state = i64::from(index + 1);
            down_event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state);
            up_event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state);
        }
        down_event.post_to_pid(target.owner_pid as i32);
        up_event.post_to_pid(target.owner_pid as i32);
        if double && index == 0 {
            sleep_unless_cancelled(cancel_requested, Duration::from_millis(70))?;
        }
    }
    Ok(())
}

fn move_pointer(
    target: &WindowRecord,
    x: f64,
    y: f64,
    cancel_requested: &AtomicBool,
) -> Result<(), ComputerUseError> {
    let point = target.frame.target_point(x, y)?;
    ensure_not_cancelled(cancel_requested)?;
    ensure_target_frontmost_at_point(target, point)?;
    let event = CGEvent::new_mouse_event(
        source()?,
        CGEventType::MouseMoved,
        cg_point(point),
        CGMouseButton::Left,
    )
    .map_err(|_| input_error())?;
    event.post(CGEventTapLocation::HID);
    Ok(())
}

fn scroll(
    target: &WindowRecord,
    x: f64,
    y: f64,
    delta_x: i32,
    delta_y: i32,
    cancel_requested: &AtomicBool,
) -> Result<(), ComputerUseError> {
    if delta_x == 0 && delta_y == 0 {
        return Err(invalid_input("scroll delta must not be zero"));
    }
    move_pointer(target, x, y, cancel_requested)?;
    ensure_not_cancelled(cancel_requested)?;
    let point = target.frame.target_point(x, y)?;
    ensure_target_frontmost_at_point(target, point)?;
    let event =
        CGEvent::new_scroll_event(source()?, ScrollEventUnit::PIXEL, 2, delta_y, delta_x, 0)
            .map_err(|_| input_error())?;
    event.post_to_pid(target.owner_pid as i32);
    Ok(())
}

fn type_text_chunk(target: &WindowRecord, chunk: &[u16]) -> Result<(), ComputerUseError> {
    let down = CGEvent::new_keyboard_event(source()?, 0, true).map_err(|_| input_error())?;
    let up = CGEvent::new_keyboard_event(source()?, 0, false).map_err(|_| input_error())?;
    unsafe {
        CGEventKeyboardSetUnicodeString(down.as_ptr(), chunk.len(), chunk.as_ptr());
        CGEventKeyboardSetUnicodeString(up.as_ptr(), chunk.len(), chunk.as_ptr());
    }
    down.post_to_pid(target.owner_pid as i32);
    up.post_to_pid(target.owner_pid as i32);
    Ok(())
}

fn key_press(
    target: &WindowRecord,
    key: &str,
    modifiers: &[KeyModifier],
    cancel_requested: &AtomicBool,
) -> Result<(), ComputerUseError> {
    ensure_not_cancelled(cancel_requested)?;
    ensure_target_frontmost_for_keyboard(target)?;
    let keycode = key_code(key).ok_or_else(|| invalid_input("unsupported key name"))?;
    let flags = modifier_flags(modifiers);
    let down = CGEvent::new_keyboard_event(source()?, keycode, true).map_err(|_| input_error())?;
    let up = CGEvent::new_keyboard_event(source()?, keycode, false).map_err(|_| input_error())?;
    down.set_flags(flags);
    up.set_flags(flags);
    down.post_to_pid(target.owner_pid as i32);
    up.post_to_pid(target.owner_pid as i32);
    Ok(())
}

fn modifier_flags(modifiers: &[KeyModifier]) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    for modifier in modifiers {
        flags |= match modifier {
            KeyModifier::Command => CGEventFlags::CGEventFlagCommand,
            KeyModifier::Control => CGEventFlags::CGEventFlagControl,
            KeyModifier::Option => CGEventFlags::CGEventFlagAlternate,
            KeyModifier::Shift => CGEventFlags::CGEventFlagShift,
            KeyModifier::Function => CGEventFlags::CGEventFlagSecondaryFn,
        };
    }
    flags
}

fn key_code(key: &str) -> Option<u16> {
    Some(match key.to_ascii_lowercase().as_str() {
        "a" => 0x00,
        "s" => 0x01,
        "d" => 0x02,
        "f" => 0x03,
        "h" => 0x04,
        "g" => 0x05,
        "z" => 0x06,
        "x" => 0x07,
        "c" => 0x08,
        "v" => 0x09,
        "b" => 0x0b,
        "q" => 0x0c,
        "w" => 0x0d,
        "e" => 0x0e,
        "r" => 0x0f,
        "y" => 0x10,
        "t" => 0x11,
        "1" => 0x12,
        "2" => 0x13,
        "3" => 0x14,
        "4" => 0x15,
        "6" => 0x16,
        "5" => 0x17,
        "=" | "equal" => 0x18,
        "9" => 0x19,
        "7" => 0x1a,
        "-" | "minus" => 0x1b,
        "8" => 0x1c,
        "0" => 0x1d,
        "]" | "right_bracket" => 0x1e,
        "o" => 0x1f,
        "u" => 0x20,
        "[" | "left_bracket" => 0x21,
        "i" => 0x22,
        "p" => 0x23,
        "l" => 0x25,
        "j" => 0x26,
        "'" | "quote" => 0x27,
        "k" => 0x28,
        ";" | "semicolon" => 0x29,
        "\\" | "backslash" => 0x2a,
        "," | "comma" => 0x2b,
        "/" | "slash" => 0x2c,
        "n" => 0x2d,
        "m" => 0x2e,
        "." | "period" => 0x2f,
        "`" | "grave" => 0x32,
        "enter" | "return" => KeyCode::RETURN,
        "tab" => KeyCode::TAB,
        "space" => KeyCode::SPACE,
        "backspace" | "delete" => KeyCode::DELETE,
        "forward_delete" => KeyCode::FORWARD_DELETE,
        "escape" | "esc" => KeyCode::ESCAPE,
        "home" => KeyCode::HOME,
        "end" => KeyCode::END,
        "page_up" => KeyCode::PAGE_UP,
        "page_down" => KeyCode::PAGE_DOWN,
        "left" => KeyCode::LEFT_ARROW,
        "right" => KeyCode::RIGHT_ARROW,
        "up" => KeyCode::UP_ARROW,
        "down" => KeyCode::DOWN_ARROW,
        "f1" => KeyCode::F1,
        "f2" => KeyCode::F2,
        "f3" => KeyCode::F3,
        "f4" => KeyCode::F4,
        "f5" => KeyCode::F5,
        "f6" => KeyCode::F6,
        "f7" => KeyCode::F7,
        "f8" => KeyCode::F8,
        "f9" => KeyCode::F9,
        "f10" => KeyCode::F10,
        "f11" => KeyCode::F11,
        "f12" => KeyCode::F12,
        _ => return None,
    })
}

fn mouse_types(button: MouseButton) -> (CGMouseButton, CGEventType, CGEventType) {
    match button {
        MouseButton::Left => (
            CGMouseButton::Left,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
        ),
        MouseButton::Right => (
            CGMouseButton::Right,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
        ),
        MouseButton::Middle => (
            CGMouseButton::Center,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
        ),
    }
}

fn cg_point(point: Point) -> CGPoint {
    CGPoint::new(point.x, point.y)
}

fn input_error() -> ComputerUseError {
    ComputerUseError::new(
        ErrorCode::InputFailed,
        "failed to create native input event",
        true,
    )
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    fn CGEventKeyboardSetUnicodeString(
        event: core_graphics::sys::CGEventRef,
        string_length: usize,
        unicode_string: *const u16,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_marker_is_private_and_removed_on_stop() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("active");
        let mut backend = MacBackend {
            target: None,
            overlay: None,
            state: ServiceState::WaitingForTarget,
            target_epoch: 0,
            coordinate_scale_x: 1.0,
            coordinate_scale_y: 1.0,
            last_capture_frame: None,
            active_marker: Some(marker.clone()),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        };
        backend.mark_active().unwrap();
        assert_eq!(
            std::fs::metadata(&marker).unwrap().permissions().mode() & 0o777,
            0o600
        );
        backend.stop().unwrap();
        assert!(!marker.exists());
        backend.cancel_requested.store(true, Ordering::Release);
        assert_eq!(
            backend.mark_active().unwrap_err().code,
            ErrorCode::TargetInvalid
        );
        assert!(!marker.exists());
    }

    #[test]
    fn cancellation_interrupts_wait_without_blocking_for_full_duration() {
        let cancel_requested = Arc::new(AtomicBool::new(true));
        let backend = MacBackend {
            target: None,
            overlay: None,
            state: ServiceState::WaitingForTarget,
            target_epoch: 0,
            coordinate_scale_x: 1.0,
            coordinate_scale_y: 1.0,
            last_capture_frame: None,
            active_marker: None,
            cancel_requested,
        };
        let started = Instant::now();
        let error = backend
            .wait_unless_cancelled(Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetInvalid);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn input_is_rejected_when_another_window_is_in_front() {
        let target = WindowRecord {
            id: 1,
            owner_pid: 10,
            owner_name: "Target".into(),
            title: None,
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
        };
        let covering = WindowRecord {
            id: 2,
            owner_pid: 20,
            owner_name: "Covering".into(),
            title: None,
            frame: Rect {
                x: 100.0,
                y: 100.0,
                width: 200.0,
                height: 200.0,
            },
        };
        let point = Point { x: 150.0, y: 150.0 };
        assert!(!target_is_frontmost_at_point(
            &[covering.clone(), target.clone()],
            &target,
            point
        ));
        assert!(target_is_frontmost_at_point(
            &[target.clone(), covering],
            &target,
            point
        ));
        assert!(!target_is_frontmost(
            &[
                WindowRecord {
                    id: 3,
                    owner_pid: 30,
                    owner_name: "Other".into(),
                    title: None,
                    frame: target.frame,
                },
                target.clone(),
            ],
            &target
        ));
        assert!(target_is_frontmost(std::slice::from_ref(&target), &target));
    }

    #[test]
    fn filters_shell_and_system_windows() {
        assert!(!eligible_owner("Dock"));
        assert!(!eligible_owner("Window Server"));
        assert!(eligible_owner("Safari"));
    }

    #[test]
    fn known_keys_map_and_unknown_keys_fail() {
        assert_eq!(key_code("ENTER"), Some(KeyCode::RETURN));
        assert_eq!(key_code("page_down"), Some(KeyCode::PAGE_DOWN));
        assert_eq!(key_code("a"), Some(0x00));
        assert_eq!(key_code("definitely_unknown"), None);
    }

    #[test]
    fn target_identity_includes_pid_to_prevent_window_id_reuse() {
        let locked = WindowRecord {
            id: 42,
            owner_pid: 7,
            owner_name: "A".into(),
            title: None,
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
        };
        let reused = WindowRecord {
            owner_pid: 8,
            ..locked.clone()
        };
        assert!(!(locked.id == reused.id && locked.owner_pid == reused.owner_pid));
    }

    #[test]
    fn state_machine_pause_resume_stop_is_strict() {
        let mut backend = MacBackend::new().unwrap();
        assert_eq!(backend.state, ServiceState::WaitingForTarget);
        backend.pause().unwrap();
        assert_eq!(backend.state, ServiceState::Paused);
        backend.resume().unwrap();
        assert_eq!(backend.state, ServiceState::WaitingForTarget);
        backend.stop().unwrap();
        assert_eq!(backend.state, ServiceState::Stopped);
        assert!(backend.pause().is_err());
    }

    #[test]
    #[ignore = "requires interactive macOS TCC grants and a real target window"]
    fn interactive_window_enumeration() {
        let backend = MacBackend::new().unwrap();
        assert!(!backend.windows().unwrap().is_empty());
    }
}
