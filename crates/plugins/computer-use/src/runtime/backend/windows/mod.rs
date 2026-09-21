//! Windows-native Computer Use backend.
//!
//! Mirrors the macOS backend's state machine and safety rules: a target is
//! acquired once per selection from a screen point, every action revalidates
//! the locked window (HWND + PID + process creation time + frame), and input
//! is refused unless the target both matches the last captured frame and can
//! be brought to the foreground. Windows has no Screen Recording/Accessibility
//! TCC gates, so both permissions always report `Granted`; UIPI integrity is
//! checked instead when the target is acquired.

mod capture;
mod input;
mod overlay;
mod security;
mod window;

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Once,
};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder, RgbaImage};
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use super::Backend;
use crate::runtime::{
    Action, ActionResponse, ComputerUseError, ErrorCode, PermissionState, PermissionStatus, Point,
    Rect, ScreenshotResponse, ServiceState, StatusResponse, TargetWindow, ACTIVE_PATH_ENV,
    MAX_TYPE_CHARS, MAX_WAIT_MS,
};
use window::WindowRecord;

use super::common::{
    action_cancelled, bounded_dimensions, capture_dimensions_error, capture_frame_matches,
    invalid_input, marker_error,
};

pub struct WindowsBackend {
    target: Option<WindowRecord>,
    overlay: Option<overlay::Overlay>,
    state: ServiceState,
    target_epoch: u64,
    coordinate_scale_x: f64,
    coordinate_scale_y: f64,
    last_capture_frame: Option<Rect>,
    active_marker: Option<PathBuf>,
    cancel_requested: Arc<AtomicBool>,
}

impl WindowsBackend {
    pub fn new() -> Result<Self, ComputerUseError> {
        Self::new_with_cancel_signal(Arc::new(AtomicBool::new(false)))
    }

    pub fn new_with_cancel_signal(
        cancel_requested: Arc<AtomicBool>,
    ) -> Result<Self, ComputerUseError> {
        ensure_dpi_awareness();
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
        Ok(window::enumerate_windows()
            .iter()
            .map(WindowRecord::public)
            .collect())
    }

    fn permissions(&self) -> PermissionStatus {
        // Windows has no OS-level capture/input consent dialogs; the effective
        // gates (UIPI integrity, foreground rules) are enforced per action.
        PermissionStatus {
            screen_recording: PermissionState::Granted,
            accessibility: PermissionState::Granted,
        }
    }

    fn clear_active_marker(&self) {
        if let Some(path) = self.active_marker.as_ref() {
            let _ = std::fs::remove_file(path);
        }
    }

    fn mark_active(&self) -> Result<(), ComputerUseError> {
        if self.cancel_requested.load(Ordering::Acquire) {
            return Err(action_cancelled());
        }
        let Some(path) = self.active_marker.as_ref() else {
            return Ok(());
        };
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                if self.cancel_requested.load(Ordering::Acquire) {
                    self.clear_active_marker();
                    return Err(action_cancelled());
                }
                return Ok(());
            }
            Ok(_) => {
                return Err(ComputerUseError::new(
                    ErrorCode::Internal,
                    "Computer Use activation marker is not a regular file",
                    false,
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(marker_error(error)),
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(marker_error)?;
        if self.cancel_requested.load(Ordering::Acquire) {
            self.clear_active_marker();
            return Err(action_cancelled());
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
        let target = window::window_at_point(point)?;
        security::ensure_target_integrity(target.owner_pid)?;
        // The highlight is feedback, not a safety boundary: a desktop that
        // refuses the layered window (rare) still gets a working session.
        self.overlay = overlay::Overlay::new(target.frame).ok();
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
        let Some(current) = window::refresh_record(&locked) else {
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
        let target = self.refresh_target()?;
        // An open menu is a separate top-level window that may hang past the
        // window's edge: capture the composited screen over the union so the
        // model sees the whole menu it just opened, not a clipped sliver.
        let region = self.capture_region(&target);
        let captured = capture::capture_region(target.raw_hwnd(), region, region != target.frame)?;
        if captured.width == 0 || captured.height == 0 {
            return Err(capture_dimensions_error());
        }
        let (output_width, output_height) = bounded_dimensions(captured.width, captured.height);
        let pixels = if (output_width, output_height) == (captured.width, captured.height) {
            captured.pixels
        } else {
            let source = RgbaImage::from_raw(captured.width, captured.height, captured.pixels)
                .ok_or_else(capture_dimensions_error)?;
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
        self.coordinate_scale_x = (f64::from(output_width) / region.width).max(f64::EPSILON);
        self.coordinate_scale_y = (f64::from(output_height) / region.height).max(f64::EPSILON);
        self.last_capture_frame = Some(region);
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

    /// The screenshot's region — and therefore the coordinate space actions
    /// use: the window frame, widened to cover any menu the target has open.
    ///
    /// Widening does not widen what may be clicked: every mouse action still
    /// has to land on the target or one of its own popups, which is checked
    /// against live window ownership at input time.
    fn capture_region(&self, target: &WindowRecord) -> Rect {
        union_rect(target.frame, &window::visible_transient_popups(target))
    }

    fn input_target(&mut self) -> Result<WindowRecord, ComputerUseError> {
        let target = self.refresh_target()?;
        if self.last_capture_frame.is_none() {
            return Err(ComputerUseError::new(
                ErrorCode::TargetInvalid,
                "observe the target before sending input",
                true,
            ));
        }
        // Compared against the *captured region*, not the bare frame: a menu
        // that closed since the screenshot moves every coordinate the model is
        // about to use, exactly like the window itself moving.
        if !capture_frame_matches(self.last_capture_frame, self.capture_region(&target)) {
            return Err(ComputerUseError::new(
                ErrorCode::TargetInvalid,
                "the target moved, resized, or closed its menu; observe it again before sending input",
                true,
            ));
        }
        Ok(target)
    }

    /// The screen point for a mouse action, validated to hit the locked
    /// window and not be covered by another window.
    fn pointer_target(
        &mut self,
        x: f64,
        y: f64,
    ) -> Result<(WindowRecord, Point), ComputerUseError> {
        let (x, y) = self.input_coordinates(x, y)?;
        let target = self.input_target()?;
        let region = self
            .last_capture_frame
            .ok_or_else(ComputerUseError::invalid_coordinates)?;
        let point = region.target_point(x, y)?;
        // A window that popped a menu keeps the menu, not itself, in the
        // foreground slot; forcing focus here would dismiss it.
        if !window::has_visible_transient_popup(&target) {
            input::ensure_foreground(target.raw_hwnd())?;
        }
        if !window::point_targets_window(point, &target) {
            return Err(ComputerUseError::new(
                ErrorCode::TargetInvalid,
                "another window covers the requested point; observe the target again",
                true,
            ));
        }
        Ok((target, point))
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
            if self.cancel_requested.load(Ordering::Acquire) {
                return Err(action_cancelled());
            }
            let target = self.input_target()?;
            self.focus_for_keyboard(&target)?;
            input::type_chunk(chunk)?;
        }
        Ok(())
    }

    /// Keyboard input needs the target focused — except while it owns an open
    /// menu, which holds the foreground itself and would be dismissed by a
    /// focus change. Menu navigation (arrows, Enter, Escape) has to reach the
    /// menu, so the keystrokes go to whatever the target popped up.
    fn focus_for_keyboard(&self, target: &WindowRecord) -> Result<(), ComputerUseError> {
        if window::has_visible_transient_popup(target) {
            return Ok(());
        }
        input::ensure_foreground(target.raw_hwnd())
    }

    fn settle_after_input(&self) -> Result<(), ComputerUseError> {
        self.wait_unless_cancelled(Duration::from_millis(100))
    }
}

impl Backend for WindowsBackend {
    fn status(&mut self) -> Result<StatusResponse, ComputerUseError> {
        if self.state == ServiceState::Stopped {
            return Ok(self.current_status());
        }
        if self.target.is_some() {
            let _ = self.refresh_target();
        }
        Ok(self.current_status())
    }

    fn execute(&mut self, action: Action) -> Result<ActionResponse, ComputerUseError> {
        if self.cancel_requested.load(Ordering::Acquire) {
            return Err(action_cancelled());
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
                let (_, point) = self.pointer_target(x, y)?;
                let screen = input::VirtualScreen::current()?;
                input::click(&screen, point, button, false)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::DoubleClick { x, y, button } => {
                let (_, point) = self.pointer_target(x, y)?;
                let screen = input::VirtualScreen::current()?;
                input::click(&screen, point, button, true)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::Move { x, y } => {
                let (_, point) = self.pointer_target(x, y)?;
                let screen = input::VirtualScreen::current()?;
                input::move_pointer(&screen, point)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::Scroll {
                x,
                y,
                delta_x,
                delta_y,
            } => {
                if delta_x == 0 && delta_y == 0 {
                    return Err(invalid_input("scroll delta must not be zero"));
                }
                let (_, point) = self.pointer_target(x, y)?;
                let screen = input::VirtualScreen::current()?;
                input::scroll(&screen, point, delta_x, delta_y)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::Type { text } => {
                if text.chars().count() > MAX_TYPE_CHARS {
                    return Err(invalid_input("text input is too long"));
                }
                self.type_text(&text)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::Key { key, modifiers } => {
                let target = self.input_target()?;
                self.focus_for_keyboard(&target)?;
                input::key_press(&key, &modifiers)?;
                self.settle_after_input()?;
                self.capture()?
            }
            Action::Wait { duration_ms } => {
                if duration_ms > MAX_WAIT_MS {
                    return Err(invalid_input("wait duration is too long"));
                }
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
            return Err(action_cancelled());
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
        // Windows has no consent dialogs to prompt; report the current state.
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

impl Drop for WindowsBackend {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The composited-screen and injection coordinate spaces only agree when the
/// process is per-monitor-v2 DPI aware. The desktop app's UI toolkit already
/// opts in; this is a no-op safety net for any other host.
fn ensure_dpi_awareness() {
    static DPI: Once = Once::new();
    DPI.call_once(|| unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    });
}

/// The screenshot region: the window frame plus every popup it has open.
/// Popups are already filtered to those overlapping the frame, so the union
/// stays anchored to the target instead of chasing a stray window.
fn union_rect(frame: Rect, popups: &[Rect]) -> Rect {
    let mut left = frame.x;
    let mut top = frame.y;
    let mut right = frame.x + frame.width;
    let mut bottom = frame.y + frame.height;
    for popup in popups {
        left = left.min(popup.x);
        top = top.min(popup.y);
        right = right.max(popup.x + popup.width);
        bottom = bottom.max(popup.y + popup.height);
    }
    Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_backend() -> WindowsBackend {
        WindowsBackend {
            target: None,
            overlay: None,
            state: ServiceState::WaitingForTarget,
            target_epoch: 0,
            coordinate_scale_x: 1.0,
            coordinate_scale_y: 1.0,
            last_capture_frame: None,
            active_marker: None,
            cancel_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn activation_marker_is_created_and_removed_on_stop() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("active");
        let mut backend = test_backend();
        backend.active_marker = Some(marker.clone());
        backend.mark_active().unwrap();
        assert!(marker.is_file());
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
        let backend = test_backend();
        backend.cancel_requested.store(true, Ordering::Release);
        let started = Instant::now();
        let error = backend
            .wait_unless_cancelled(Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetInvalid);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn capture_region_covers_the_frame_and_any_open_menu() {
        let frame = Rect {
            x: 200.0,
            y: 100.0,
            width: 300.0,
            height: 160.0,
        };
        assert_eq!(union_rect(frame, &[]), frame);

        // A File menu dropping below the window's bottom edge.
        let dropdown = Rect {
            x: 210.0,
            y: 130.0,
            width: 180.0,
            height: 300.0,
        };
        assert_eq!(
            union_rect(frame, &[dropdown]),
            Rect {
                x: 200.0,
                y: 100.0,
                width: 300.0,
                height: 330.0
            }
        );

        // A submenu flying out to the left of the window keeps its origin.
        let submenu = Rect {
            x: 40.0,
            y: 150.0,
            width: 200.0,
            height: 120.0,
        };
        assert_eq!(
            union_rect(frame, &[dropdown, submenu]),
            Rect {
                x: 40.0,
                y: 100.0,
                width: 460.0,
                height: 330.0
            }
        );

        // A menu entirely inside the window never shrinks the region.
        let inline = Rect {
            x: 220.0,
            y: 120.0,
            width: 60.0,
            height: 60.0,
        };
        assert_eq!(union_rect(frame, &[inline]), frame);
    }

    #[test]
    fn closing_a_menu_invalidates_screenshot_coordinates() {
        let frame = Rect {
            x: 200.0,
            y: 100.0,
            width: 300.0,
            height: 160.0,
        };
        let with_menu = union_rect(
            frame,
            &[Rect {
                x: 210.0,
                y: 130.0,
                width: 180.0,
                height: 300.0,
            }],
        );
        // The screenshot was taken with the menu open; once it closes the
        // region shrinks back and every coordinate the model derived from that
        // screenshot points somewhere else.
        assert!(!capture_frame_matches(Some(with_menu), frame));
        assert!(capture_frame_matches(Some(with_menu), with_menu));
    }

    #[test]
    fn state_machine_pause_resume_stop_is_strict() {
        let mut backend = WindowsBackend::new().unwrap();
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
    fn permissions_are_always_granted_on_windows() {
        let backend = test_backend();
        let permissions = backend.permissions();
        assert_eq!(permissions.screen_recording, PermissionState::Granted);
        assert_eq!(permissions.accessibility, PermissionState::Granted);
    }

    #[test]
    fn input_requires_an_observation_first() {
        let mut backend = test_backend();
        backend.target = Some(WindowRecord {
            hwnd: 0x10,
            owner_pid: std::process::id(),
            process_created: 1,
            owner_name: "Test".into(),
            title: None,
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
        });
        // The locked HWND does not exist, so refresh must invalidate it.
        let error = backend.input_target().unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetInvalid);
        assert!(backend.target.is_none());
        assert_eq!(backend.state, ServiceState::WaitingForTarget);
    }

    /// Drives an application menu the way the model does — Alt to focus the
    /// menu bar, Down to drop it open, Escape to dismiss — and proves the
    /// dropdown reaches the screenshot.
    ///
    /// Launches its own Notepad so the target is guaranteed visible, on top,
    /// and equipped with a classic `#32768` menu; `REBON_CU_E2E_PNG_DIR` saves
    /// each frame. Without the popup-aware capture path every one of these
    /// screenshots shows a menu-less window.
    #[test]
    #[ignore = "interactive: launches Notepad, opens its menu, and captures it"]
    fn interactive_menu_capture_and_navigation() {
        // Construct the backend *first*: it opts the process into per-monitor
        // DPI awareness, and window rectangles read before that are virtualized
        // — picking a point from them locks whatever sits at the physical
        // coordinate instead of the intended window.
        let mut backend = WindowsBackend::new().unwrap();
        let mut notepad = std::process::Command::new("notepad.exe")
            .spawn()
            .expect("failed to launch notepad");
        let _reap = ReapOnDrop(&mut notepad);
        let target = wait_for_window(_reap.0.id()).expect("notepad window never appeared");
        eprintln!(
            "target: {} pid={} title={:?} frame={:?}",
            target.owner_name, target.owner_pid, target.title, target.frame
        );

        let locked = backend
            .execute(Action::Observe {
                target: Some(Point {
                    x: target.frame.x + target.frame.width / 2.0,
                    y: target.frame.y + target.frame.height / 2.0,
                }),
            })
            .expect("observe failed")
            .status
            .target
            .expect("no locked target");
        assert_eq!(
            locked.owner_pid, target.owner_pid,
            "locked the wrong window"
        );
        let record = backend.target.clone().expect("target locked");

        let save = |label: &str, response: &ActionResponse| {
            let popup = window::has_visible_transient_popup(&record);
            let shot = response.screenshot.as_ref().unwrap();
            eprintln!(
                "{label}: {}x{} popup_visible={popup}",
                shot.width, shot.height
            );
            let png = {
                use base64::Engine as _;
                BASE64.decode(&shot.png_base64).unwrap()
            };
            if let Some(directory) = std::env::var_os("REBON_CU_E2E_PNG_DIR") {
                std::fs::write(PathBuf::from(directory).join(format!("{label}.png")), &png)
                    .unwrap();
            }
            (popup, png)
        };

        let (_, closed_png) = save(
            "0-locked",
            &backend
                .execute(Action::Observe { target: None })
                .expect("observe failed"),
        );
        backend
            .execute(Action::Key {
                key: "alt".into(),
                modifiers: Vec::new(),
            })
            .expect("alt failed");
        let stepped = backend
            .execute(Action::Key {
                key: "arrowdown".into(),
                modifiers: Vec::new(),
            })
            .expect("arrowdown failed");
        let (popup_visible, open_png) = save("1-menu-open", &stepped);
        assert!(
            popup_visible,
            "no menu popup was detected; the capture path would have missed it"
        );
        assert_ne!(
            open_png, closed_png,
            "the open menu did not change the screenshot"
        );

        // Clicking an item *inside* the open menu. Before menus were
        // recognised as the target's own surface this was refused as "another
        // window covers the requested point", which made every menu unusable.
        // The menu has to still be open, so this runs before dismissing it.
        let popup = *window::visible_transient_popups(&record)
            .first()
            .expect("no menu to click");
        let region = backend.last_capture_frame.expect("no capture region");
        let clicked = backend
            .execute(Action::Click {
                x: (popup.x + popup.width / 2.0 - region.x) * backend.coordinate_scale_x,
                y: (popup.y + popup.height / 2.0 - region.y) * backend.coordinate_scale_y,
                button: crate::runtime::MouseButton::Left,
            })
            .expect("clicking a menu item was refused");
        let (_, clicked_png) = save("2-menu-item-clicked", &clicked);
        assert_ne!(
            open_png, clicked_png,
            "clicking the menu item changed nothing on screen"
        );
    }

    struct ReapOnDrop<'a>(&'a mut std::process::Child);

    impl Drop for ReapOnDrop<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn wait_for_window(pid: u32) -> Option<WindowRecord> {
        for _ in 0..100 {
            if let Some(found) = window::enumerate_windows()
                .into_iter()
                .find(|candidate| candidate.owner_pid == pid)
            {
                // Let the window finish laying out before it is captured.
                std::thread::sleep(Duration::from_millis(200));
                return window::refresh_record(&found);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    #[test]
    #[ignore = "requires an interactive Windows session with real windows"]
    fn interactive_window_enumeration() {
        let backend = WindowsBackend::new().unwrap();
        assert!(!backend.windows().unwrap().is_empty());
    }

    /// Full stack minus the click picker: real window enumeration, a live
    /// named-pipe server, and an `observe` that locks + captures the topmost
    /// eligible window. Set `REBON_CU_E2E_PNG=<path>` to save the screenshot.
    #[test]
    #[ignore = "interactive: locks and captures a real desktop window over IPC"]
    fn interactive_end_to_end_observe() {
        let windows = window::enumerate_windows();
        assert!(!windows.is_empty(), "no eligible windows on this desktop");
        for candidate in &windows {
            eprintln!(
                "eligible: {} pid={} title={:?} frame={:?}",
                candidate.owner_name, candidate.owner_pid, candidate.title, candidate.frame
            );
        }
        let target = &windows[0];
        let point = Point {
            x: target.frame.x + target.frame.width / 2.0,
            y: target.frame.y + target.frame.height / 2.0,
        };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let pipe = PathBuf::from(format!(r"\\.\pipe\rebon-cu-e2e-{}", std::process::id()));
        let backend = WindowsBackend::new().unwrap();
        let server_pipe = pipe.clone();
        runtime.spawn(async move {
            let _ = crate::runtime::ipc::serve(server_pipe, "secret", backend).await;
        });
        for _ in 0..200 {
            if crate::runtime::ipc::endpoint_ready(&pipe) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            crate::runtime::ipc::endpoint_ready(&pipe),
            "pipe never became ready"
        );

        let response = runtime
            .block_on(crate::runtime::ipc::Client::new(&pipe, "secret").request(
                crate::runtime::Request::Action {
                    action: Action::Observe {
                        target: Some(point),
                    },
                    target_epoch: None,
                },
            ))
            .expect("observe over IPC failed");
        let locked = response.status.target.expect("no locked target");
        let shot = response.screenshot.expect("no screenshot in response");
        eprintln!(
            "locked \"{}\" ({}) -> {}x{} scale {:.3}",
            locked.owner_name, locked.owner_pid, shot.width, shot.height, shot.scale
        );
        assert!(shot.width > 0 && shot.height > 0);
        if let Some(path) = std::env::var_os("REBON_CU_E2E_PNG") {
            use base64::Engine as _;
            let png = BASE64.decode(shot.png_base64).unwrap();
            std::fs::write(path, png).unwrap();
        }
    }
}
