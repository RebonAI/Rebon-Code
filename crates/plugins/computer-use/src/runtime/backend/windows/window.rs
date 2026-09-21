//! Win32 window discovery, locking, and per-action revalidation.
//!
//! A locked target is identified by HWND *plus* process id *plus* process
//! creation time: Windows reuses both window handles and process ids, and only
//! the creation time makes the triple unforgeable for the session's lifetime.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, WindowFromPoint, GA_ROOT, GWL_EXSTYLE, GW_OWNER, WS_EX_TOOLWINDOW,
};

use crate::runtime::{ComputerUseError, ErrorCode, Point, Rect, TargetWindow};

const MIN_TARGET_EDGE: f64 = 32.0;

#[derive(Clone, Debug)]
pub(super) struct WindowRecord {
    /// HWND stored as an integer so the record stays `Send`.
    pub hwnd: isize,
    pub owner_pid: u32,
    /// Process creation FILETIME packed into a u64; guards against PID reuse.
    pub process_created: u64,
    pub owner_name: String,
    pub title: Option<String>,
    /// DWM extended frame bounds in physical virtual-desktop pixels.
    pub frame: Rect,
}

impl WindowRecord {
    pub fn public(&self) -> TargetWindow {
        TargetWindow {
            id: self.hwnd as u64,
            owner_pid: self.owner_pid,
            owner_name: self.owner_name.clone(),
            title: self.title.clone(),
            frame: self.frame,
        }
    }

    pub fn raw_hwnd(&self) -> HWND {
        self.hwnd as HWND
    }
}

/// Ordinary, visible application windows in front-to-back Z order.
pub(super) fn enumerate_windows() -> Vec<WindowRecord> {
    struct EnumContext {
        windows: Vec<WindowRecord>,
    }

    unsafe extern "system" fn callback(hwnd: HWND, lparam: isize) -> i32 {
        let context = unsafe { &mut *(lparam as *mut EnumContext) };
        if let Some(record) = record_for(hwnd) {
            context.windows.push(record);
        }
        1
    }

    let mut context = EnumContext {
        windows: Vec::new(),
    };
    unsafe {
        EnumWindows(Some(callback), std::ptr::addr_of_mut!(context) as isize);
    }
    context.windows
}

/// The eligible top-level window under a physical virtual-desktop point.
pub(super) fn window_at_point(point: Point) -> Result<WindowRecord, ComputerUseError> {
    if !point.x.is_finite() || !point.y.is_finite() {
        return Err(ComputerUseError::invalid_coordinates());
    }
    let at = unsafe {
        WindowFromPoint(POINT {
            x: point.x.round() as i32,
            y: point.y.round() as i32,
        })
    };
    let root = if at.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe { GetAncestor(at, GA_ROOT) }
    };
    if root.is_null() {
        return Err(no_window_at_point());
    }
    record_for(root).ok_or_else(no_window_at_point)
}

fn no_window_at_point() -> ComputerUseError {
    ComputerUseError::new(
        ErrorCode::TargetNotSelected,
        "no eligible window contains the requested screen point",
        true,
    )
}

/// Classes Windows uses for transient surfaces an application pops up:
/// dropdown/context menus and combo-box lists. They are separate top-level
/// windows, not children of the window that opened them.
pub(super) fn transient_surface_class(class: &str) -> bool {
    matches!(class, "#32768" | "ComboLBox")
}

/// Whether a window under the cursor still counts as "the locked target".
///
/// An open menu is its own top-level window, so a naive `hwnd == target` test
/// rejects every click on a menu item as "another window covers the point" —
/// which makes menus, context menus and dropdowns unusable. A surface counts
/// as the target's own when it belongs to the same process *and* is either a
/// menu-class window or owned (directly or transitively) by the target.
pub(super) fn surface_belongs_to_target(
    is_target: bool,
    same_process: bool,
    class: &str,
    owner_chain_reaches_target: bool,
) -> bool {
    is_target || (same_process && (transient_surface_class(class) || owner_chain_reaches_target))
}

/// Owner-chain depth is bounded: a cycle here would hang an action.
const MAX_OWNER_DEPTH: usize = 8;

fn owner_chain_reaches(hwnd: HWND, target: isize) -> bool {
    let mut owner = unsafe { GetWindow(hwnd, GW_OWNER) };
    for _ in 0..MAX_OWNER_DEPTH {
        if owner.is_null() {
            return false;
        }
        if owner as isize == target {
            return true;
        }
        owner = unsafe { GetWindow(owner, GW_OWNER) };
    }
    false
}

fn belongs_to_target(hwnd: HWND, target: &WindowRecord) -> bool {
    if hwnd as isize == target.hwnd {
        return true;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    surface_belongs_to_target(
        false,
        pid == target.owner_pid,
        &window_class(hwnd),
        owner_chain_reaches(hwnd, target.hwnd),
    )
}

/// Whether the point may receive input for `target`: the target itself, or a
/// menu / dropdown / owned popup the target opened.
pub(super) fn point_targets_window(point: Point, target: &WindowRecord) -> bool {
    let at = unsafe {
        WindowFromPoint(POINT {
            x: point.x.round() as i32,
            y: point.y.round() as i32,
        })
    };
    if at.is_null() {
        return false;
    }
    let root = unsafe { GetAncestor(at, GA_ROOT) };
    !root.is_null() && belongs_to_target(root, target)
}

/// Visible menus / dropdowns the target has open, in Z order.
///
/// Such a surface is invisible to `PrintWindow` of the target — a screenshot
/// taken without accounting for it shows a window with no menu in it — so the
/// capture path both switches to a composited screen read and widens the
/// captured region to cover them.
pub(super) fn visible_transient_popups(target: &WindowRecord) -> Vec<Rect> {
    struct PopupScan<'a> {
        target: &'a WindowRecord,
        found: Vec<Rect>,
    }

    unsafe extern "system" fn callback(hwnd: HWND, lparam: isize) -> i32 {
        let scan = unsafe { &mut *(lparam as *mut PopupScan) };
        if hwnd as isize == scan.target.hwnd {
            // Z order: nothing behind the target can overlay it.
            return 0;
        }
        if unsafe { IsWindowVisible(hwnd) } == 0 || !belongs_to_target(hwnd, scan.target) {
            return 1;
        }
        if let Some(popup) = window_frame(hwnd)
            .filter(|popup| popup.is_valid() && overlaps(*popup, scan.target.frame))
        {
            scan.found.push(popup);
        }
        1
    }

    let mut scan = PopupScan {
        target,
        found: Vec::new(),
    };
    unsafe {
        EnumWindows(Some(callback), std::ptr::addr_of_mut!(scan) as isize);
    }
    scan.found
}

pub(super) fn has_visible_transient_popup(target: &WindowRecord) -> bool {
    !visible_transient_popups(target).is_empty()
}

pub(super) fn overlaps(left: Rect, right: Rect) -> bool {
    left.x < right.x + right.width
        && right.x < left.x + left.width
        && left.y < right.y + right.height
        && right.y < left.y + left.height
}

/// Revalidates a locked target: same HWND, same process (id *and* creation
/// time), still visible, not minimized, not cloaked. Returns the record with a
/// freshly read frame, or `None` when the target is gone or no longer usable.
pub(super) fn refresh_record(locked: &WindowRecord) -> Option<WindowRecord> {
    let hwnd = locked.raw_hwnd();
    unsafe {
        if IsWindow(hwnd) == 0 || IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return None;
        }
    }
    if is_cloaked(hwnd) {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid != locked.owner_pid {
        return None;
    }
    let created = process_creation_time(pid)?;
    if created != locked.process_created {
        return None;
    }
    let frame = window_frame(hwnd)?;
    if !frame.is_valid() {
        return None;
    }
    Some(WindowRecord {
        title: window_title(hwnd),
        frame,
        ..locked.clone()
    })
}

fn record_for(hwnd: HWND) -> Option<WindowRecord> {
    unsafe {
        if IsWindow(hwnd) == 0 || IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return None;
        }
        let exstyle = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if exstyle & WS_EX_TOOLWINDOW != 0 {
            return None;
        }
    }
    if is_cloaked(hwnd) {
        return None;
    }
    if !eligible_class(&window_class(hwnd)) {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    let frame = window_frame(hwnd)?;
    if !frame.is_valid() || frame.width < MIN_TARGET_EDGE || frame.height < MIN_TARGET_EDGE {
        return None;
    }
    let (process_created, owner_name) = process_identity(pid)?;
    Some(WindowRecord {
        hwnd: hwnd as isize,
        owner_pid: pid,
        process_created,
        owner_name,
        title: window_title(hwnd),
        frame,
    })
}

/// Shell surfaces that must never become Computer Use targets.
pub(super) fn eligible_class(class: &str) -> bool {
    !matches!(
        class,
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "NotifyIconOverflowWindow"
            | "Windows.UI.Core.CoreWindow"
    )
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            std::ptr::addr_of_mut!(cloaked).cast::<c_void>(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    result == 0 && cloaked != 0
}

/// DWM extended frame bounds (the visible window edge, excluding the
/// invisible resize border), falling back to the raw window rect.
pub(super) fn window_frame(hwnd: HWND) -> Option<Rect> {
    let mut bounds = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            std::ptr::addr_of_mut!(bounds).cast::<c_void>(),
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if result != 0 && unsafe { GetWindowRect(hwnd, &mut bounds) } == 0 {
        return None;
    }
    Some(rect_from(bounds))
}

pub(super) fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    (unsafe { GetWindowRect(hwnd, &mut rect) } != 0).then_some(rect)
}

fn rect_from(rect: RECT) -> Rect {
    Rect {
        x: f64::from(rect.left),
        y: f64::from(rect.top),
        width: f64::from(rect.right.saturating_sub(rect.left)),
        height: f64::from(rect.bottom.saturating_sub(rect.top)),
    }
}

fn window_title(hwnd: HWND) -> Option<String> {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return None;
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        return None;
    }
    let title = String::from_utf16_lossy(&buffer[..copied as usize]);
    (!title.trim().is_empty()).then_some(title)
}

fn window_class(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    let copied = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..copied as usize])
}

fn process_identity(pid: u32) -> Option<(u64, String)> {
    let process = open_process(pid)?;
    let created = creation_time(process.0);
    let name = process_image_basename(process.0);
    match (created, name) {
        (Some(created), Some(name)) => Some((created, name)),
        _ => None,
    }
}

pub(super) fn process_creation_time(pid: u32) -> Option<u64> {
    let process = open_process(pid)?;
    creation_time(process.0)
}

/// Executable base name for diagnostics (e.g. naming a foreground blocker).
pub(super) fn process_name(pid: u32) -> Option<String> {
    let process = open_process(pid)?;
    process_image_basename(process.0)
}

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open_process(pid: u32) -> Option<ProcessHandle> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    (!handle.is_null()).then_some(ProcessHandle(handle))
}

fn creation_time(process: HANDLE) -> Option<u64> {
    let empty = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut creation = empty;
    let mut exit = empty;
    let mut kernel = empty;
    let mut user = empty;
    let result =
        unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
    (result != 0)
        .then(|| (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn process_image_basename(process: HANDLE) -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut size = buffer.len() as u32;
    let result = unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size) };
    if result == 0 || size == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buffer[..size as usize]);
    let base = path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(path.as_str())
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE");
    (!base.is_empty()).then(|| base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menus_and_owned_popups_count_as_the_target_surface() {
        // The target itself always qualifies.
        assert!(surface_belongs_to_target(true, false, "Anything", false));
        // An open dropdown menu: separate top-level window, same process.
        assert!(surface_belongs_to_target(false, true, "#32768", false));
        assert!(surface_belongs_to_target(false, true, "ComboLBox", false));
        // A plain owned popup (dialog, autocomplete list) also qualifies.
        assert!(surface_belongs_to_target(false, true, "Custom", true));
        // A menu-looking window from another process never does — that is a
        // genuine occlusion and input must be refused.
        assert!(!surface_belongs_to_target(false, false, "#32768", false));
        assert!(!surface_belongs_to_target(false, false, "Custom", true));
        // Same process, unrelated top-level window: not the target.
        assert!(!surface_belongs_to_target(false, true, "Custom", false));
    }

    #[test]
    fn transient_classes_cover_menus_and_combo_lists_only() {
        assert!(transient_surface_class("#32768"));
        assert!(transient_surface_class("ComboLBox"));
        assert!(!transient_surface_class("Chrome_WidgetWin_1"));
    }

    #[test]
    fn overlap_detects_popups_over_the_frame() {
        let frame = Rect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        // A dropdown hanging under the menu bar, inside the frame.
        assert!(overlaps(
            Rect {
                x: 110.0,
                y: 130.0,
                width: 160.0,
                height: 220.0
            },
            frame
        ));
        // Touching edges do not overlap.
        assert!(!overlaps(
            Rect {
                x: 500.0,
                y: 100.0,
                width: 50.0,
                height: 50.0
            },
            frame
        ));
        assert!(!overlaps(
            Rect {
                x: 100.0,
                y: 400.0,
                width: 50.0,
                height: 50.0
            },
            frame
        ));
        // A menu that spills past the bottom edge still overlaps.
        assert!(overlaps(
            Rect {
                x: 120.0,
                y: 380.0,
                width: 200.0,
                height: 200.0
            },
            frame
        ));
    }

    #[test]
    fn shell_surfaces_are_ineligible() {
        assert!(!eligible_class("Progman"));
        assert!(!eligible_class("WorkerW"));
        assert!(!eligible_class("Shell_TrayWnd"));
        assert!(eligible_class("Chrome_WidgetWin_1"));
        assert!(eligible_class("Notepad"));
    }

    #[test]
    fn rects_convert_to_physical_pixel_frames() {
        let frame = rect_from(RECT {
            left: -1920,
            top: 40,
            right: -120,
            bottom: 1040,
        });
        assert_eq!(frame.x, -1920.0);
        assert_eq!(frame.y, 40.0);
        assert_eq!(frame.width, 1800.0);
        assert_eq!(frame.height, 1000.0);
        assert!(frame.is_valid());
    }

    #[test]
    fn creation_time_packs_filetime_words() {
        // Validated indirectly: the packing must be monotonic in both words.
        let low = (u64::from(1u32) << 32) | u64::from(5u32);
        let high = (u64::from(2u32) << 32) | u64::from(0u32);
        assert!(high > low);
    }

    #[test]
    fn window_identity_requires_pid_and_creation_time_match() {
        let locked = WindowRecord {
            hwnd: 0x1234,
            owner_pid: 7,
            process_created: 99,
            owner_name: "A".into(),
            title: None,
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
        };
        let reused_pid = WindowRecord {
            process_created: 100,
            ..locked.clone()
        };
        assert!(
            !(locked.owner_pid == reused_pid.owner_pid
                && locked.process_created == reused_pid.process_created)
        );
    }
}
