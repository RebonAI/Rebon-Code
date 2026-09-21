//! Target-window highlight ring (the Windows sibling of the macOS overlay).
//!
//! A click-through, non-activating layered popup shaped into a border ring via
//! `SetWindowRgn`. The window lives on its own message-loop thread; every
//! mutation from the backend worker is marshalled to that thread with
//! `PostMessageW`, which never blocks, so the overlay can never stall an
//! action even if the desktop is busy.

use std::sync::mpsc;
use std::sync::Once;
use std::thread;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, CreateSolidBrush, DeleteObject, SetWindowRgn, RGN_DIFF,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetLayeredWindowAttributes, SetWindowPos, TranslateMessage,
    HWND_TOPMOST, LWA_ALPHA, MSG, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, WM_APP,
    WM_CLOSE, WM_DESTROY, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::runtime::{ComputerUseError, ErrorCode, Rect};

/// Ring geometry and color mirror the macOS overlay: 7px of breathing room
/// around the target frame, a 4px accent-blue border.
const PADDING: i32 = 7;
const BORDER: i32 = 4;
/// sRGB (33, 199, 255) as a COLORREF (0x00BBGGRR).
const ACCENT: u32 = 33 | (199 << 8) | (255 << 16);

const ALPHA_NORMAL: u8 = 199; // 0.78
const ALPHA_DIM: u8 = 56; // 0.22
const ALPHA_PULSE: u8 = 255;
const PULSE_HOLD: std::time::Duration = std::time::Duration::from_millis(55);

/// Reposition + reshape. `wparam` packs x/y, `lparam` packs width/height.
const MSG_FOLLOW: u32 = WM_APP;
/// Set the layer alpha carried in `wparam` and re-assert topmost order.
const MSG_ALPHA: u32 = WM_APP + 1;

pub(super) struct Overlay {
    hwnd: isize,
    stopped: bool,
    thread: Option<thread::JoinHandle<()>>,
}

impl Overlay {
    pub fn new(frame: Rect) -> Result<Self, ComputerUseError> {
        if !frame.is_valid() {
            return Err(ComputerUseError::invalid_coordinates());
        }
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("rebon-computer-use-overlay".into())
            .spawn(move || run_overlay_window(frame, ready_tx))
            .map_err(|_| overlay_error())?;
        match ready_rx.recv() {
            Ok(Ok(hwnd)) => Ok(Self {
                hwnd,
                stopped: false,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(overlay_error()),
        }
    }

    pub fn follow(&mut self, frame: Rect) {
        if self.stopped || !frame.is_valid() {
            return;
        }
        let bounds = OverlayBounds::around(frame);
        let wparam = pack_pair(bounds.x, bounds.y);
        let lparam = pack_pair(bounds.width, bounds.height);
        unsafe {
            PostMessageW(
                self.hwnd as HWND,
                MSG_FOLLOW,
                wparam as WPARAM,
                lparam as LPARAM,
            );
        }
    }

    pub fn normal(&mut self) {
        self.set_alpha(ALPHA_NORMAL);
    }

    pub fn pulse(&mut self) {
        self.set_alpha(ALPHA_PULSE);
        std::thread::sleep(PULSE_HOLD);
        self.normal();
    }

    pub fn dim(&mut self) {
        self.set_alpha(ALPHA_DIM);
    }

    fn set_alpha(&mut self, alpha: u8) {
        if self.stopped {
            return;
        }
        unsafe {
            PostMessageW(self.hwnd as HWND, MSG_ALPHA, alpha as WPARAM, 0);
        }
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        unsafe {
            PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0);
        }
        self.stopped = true;
        self.hwnd = 0;
        // The overlay thread exits right after processing WM_CLOSE; there is
        // nothing to wait for, and joining from the backend worker would only
        // add a stall risk.
        let _ = self.thread.take();
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OverlayBounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl OverlayBounds {
    fn around(frame: Rect) -> Self {
        Self {
            x: frame.x.round() as i32 - PADDING,
            y: frame.y.round() as i32 - PADDING,
            width: frame.width.round() as i32 + PADDING * 2,
            height: frame.height.round() as i32 + PADDING * 2,
        }
    }
}

/// Two i32s in one pointer-sized message argument.
fn pack_pair(low: i32, high: i32) -> u64 {
    (low as u32 as u64) | ((high as u32 as u64) << 32)
}

fn unpack_pair(packed: u64) -> (i32, i32) {
    (packed as u32 as i32, (packed >> 32) as u32 as i32)
}

fn overlay_error() -> ComputerUseError {
    ComputerUseError::new(
        ErrorCode::Internal,
        "failed to create Computer Use overlay",
        true,
    )
}

fn class_name() -> &'static [u16] {
    // "RebonCUOverlay\0"
    const NAME: &[u16] = &[
        b'R' as u16,
        b'e' as u16,
        b'b' as u16,
        b'o' as u16,
        b'n' as u16,
        b'C' as u16,
        b'U' as u16,
        b'O' as u16,
        b'v' as u16,
        b'e' as u16,
        b'r' as u16,
        b'l' as u16,
        b'a' as u16,
        b'y' as u16,
        0,
    ];
    NAME
}

fn ensure_window_class() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(overlay_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(std::ptr::null()),
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            // Owned by the class for the process lifetime; never deleted.
            hbrBackground: CreateSolidBrush(ACCENT),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name().as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&class);
    });
}

/// Shapes the window into a `BORDER`-wide ring. The system takes ownership of
/// the region handle passed to `SetWindowRgn`.
unsafe fn apply_ring_region(hwnd: HWND, width: i32, height: i32) {
    let outer = unsafe { CreateRectRgn(0, 0, width, height) };
    let inner = unsafe {
        CreateRectRgn(
            BORDER,
            BORDER,
            (width - BORDER).max(BORDER),
            (height - BORDER).max(BORDER),
        )
    };
    unsafe {
        CombineRgn(outer, outer, inner, RGN_DIFF);
        DeleteObject(inner);
        SetWindowRgn(hwnd, outer, 1);
    }
}

unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        MSG_FOLLOW => {
            let (x, y) = unpack_pair(wparam as u64);
            let (width, height) = unpack_pair(lparam as u64);
            unsafe {
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    x,
                    y,
                    width,
                    height,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                apply_ring_region(hwnd, width, height);
            }
            0
        }
        MSG_ALPHA => {
            unsafe {
                SetLayeredWindowAttributes(hwnd, 0, wparam as u8, LWA_ALPHA);
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            0
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn run_overlay_window(frame: Rect, ready: mpsc::Sender<Result<isize, ComputerUseError>>) {
    ensure_window_class();
    let bounds = OverlayBounds::around(frame);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            class_name().as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        let _ = ready.send(Err(overlay_error()));
        return;
    }
    unsafe {
        apply_ring_region(hwnd, bounds.width, bounds.height);
        SetLayeredWindowAttributes(hwnd, 0, ALPHA_NORMAL, LWA_ALPHA);
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
            SWP_SHOWWINDOW | SWP_NOACTIVATE,
        );
    }
    let _ = ready.send(Ok(hwnd as isize));

    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_pad_the_frame_and_survive_negative_origins() {
        let bounds = OverlayBounds::around(Rect {
            x: -1920.0,
            y: -240.0,
            width: 800.0,
            height: 600.0,
        });
        assert_eq!(
            bounds,
            OverlayBounds {
                x: -1927,
                y: -247,
                width: 814,
                height: 614
            }
        );
    }

    #[test]
    fn message_packing_round_trips_negative_coordinates() {
        for (low, high) in [(0, 0), (-1927, -247), (i32::MAX, i32::MIN), (814, 614)] {
            assert_eq!(unpack_pair(pack_pair(low, high)), (low, high));
        }
    }

    #[test]
    #[ignore = "creates a real layered window; requires an interactive session"]
    fn interactive_overlay_lifecycle() {
        let mut overlay = Overlay::new(Rect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        })
        .unwrap();
        overlay.dim();
        overlay.pulse();
        overlay.follow(Rect {
            x: 200.0,
            y: 150.0,
            width: 500.0,
            height: 350.0,
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        overlay.stop();
    }
}
