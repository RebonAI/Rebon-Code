//! Input injection through `SendInput`.
//!
//! Mouse coordinates are normalized against the whole virtual desktop
//! (`MOUSEEVENTF_VIRTUALDESK`), so multi-monitor layouts with negative origins
//! inject at the right physical pixel. Keyboard text uses `KEYEVENTF_UNICODE`
//! and is therefore layout-independent.

use std::time::Duration;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F10, VK_F11, VK_F12,
    VK_F2, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_HOME, VK_INSERT, VK_LEFT, VK_LWIN,
    VK_MENU, VK_NEXT, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7,
    VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PRIOR, VK_RETURN, VK_RIGHT,
    VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, IsIconic,
    SetForegroundWindow, ShowWindow, GA_ROOT, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_RESTORE,
};

use crate::runtime::{ComputerUseError, ErrorCode, KeyModifier, MouseButton, Point};

/// One wheel notch (`WHEEL_DELTA`) corresponds to roughly 40 protocol pixels,
/// keeping scroll distance comparable with the macOS pixel-scroll backend.
const WHEEL_PIXELS_PER_NOTCH: f64 = 40.0;
const WHEEL_DELTA: f64 = 120.0;
const FOREGROUND_WAIT: Duration = Duration::from_millis(400);
const FOREGROUND_POLL: Duration = Duration::from_millis(25);
const DOUBLE_CLICK_GAP: Duration = Duration::from_millis(60);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct VirtualScreen {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl VirtualScreen {
    pub fn current() -> Result<Self, ComputerUseError> {
        let screen = unsafe {
            Self {
                x: f64::from(GetSystemMetrics(SM_XVIRTUALSCREEN)),
                y: f64::from(GetSystemMetrics(SM_YVIRTUALSCREEN)),
                width: f64::from(GetSystemMetrics(SM_CXVIRTUALSCREEN)),
                height: f64::from(GetSystemMetrics(SM_CYVIRTUALSCREEN)),
            }
        };
        if screen.width < 1.0 || screen.height < 1.0 {
            return Err(input_failed("virtual desktop metrics are unavailable"));
        }
        Ok(screen)
    }

    /// Physical virtual-desktop pixel → `SendInput` absolute 0..=65535 space.
    pub fn normalize(&self, point: Point) -> Result<(i32, i32), ComputerUseError> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(ComputerUseError::invalid_coordinates());
        }
        let scale = |value: f64, origin: f64, extent: f64| -> i32 {
            let span = (extent - 1.0).max(1.0);
            (((value - origin) * 65535.0 / span).round() as i64).clamp(0, 65535) as i32
        };
        Ok((
            scale(point.x, self.x, self.width),
            scale(point.y, self.y, self.height),
        ))
    }
}

fn mouse_input(dx: i32, dy: i32, mouse_data: i32, flags: u32) -> INPUT {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi.dx = dx;
    input.Anonymous.mi.dy = dy;
    input.Anonymous.mi.mouseData = mouse_data as _;
    input.Anonymous.mi.dwFlags = flags;
    input
}

fn key_input(vk: u16, scan: u16, flags: u32) -> INPUT {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki.wVk = vk;
    input.Anonymous.ki.wScan = scan;
    input.Anonymous.ki.dwFlags = flags;
    input
}

fn send_inputs(inputs: &[INPUT]) -> Result<(), ComputerUseError> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if sent as usize != inputs.len() {
        return Err(input_failed(
            "the system rejected the injected input (blocked by UIPI?)",
        ));
    }
    Ok(())
}

pub(super) fn move_pointer(screen: &VirtualScreen, point: Point) -> Result<(), ComputerUseError> {
    let (nx, ny) = screen.normalize(point)?;
    send_inputs(&[mouse_input(
        nx,
        ny,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )])
}

pub(super) fn click(
    screen: &VirtualScreen,
    point: Point,
    button: MouseButton,
    double: bool,
) -> Result<(), ComputerUseError> {
    move_pointer(screen, point)?;
    let (down, up) = button_flags(button);
    let (nx, ny) = screen.normalize(point)?;
    let position = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
    let press = [
        mouse_input(nx, ny, 0, position | down),
        mouse_input(nx, ny, 0, position | up),
    ];
    send_inputs(&press)?;
    if double {
        std::thread::sleep(DOUBLE_CLICK_GAP);
        send_inputs(&press)?;
    }
    Ok(())
}

pub(super) fn scroll(
    screen: &VirtualScreen,
    point: Point,
    delta_x: i32,
    delta_y: i32,
) -> Result<(), ComputerUseError> {
    move_pointer(screen, point)?;
    let (nx, ny) = screen.normalize(point)?;
    let position = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
    let mut inputs = Vec::new();
    if delta_y != 0 {
        inputs.push(mouse_input(
            nx,
            ny,
            wheel_amount(delta_y),
            position | MOUSEEVENTF_WHEEL,
        ));
    }
    if delta_x != 0 {
        inputs.push(mouse_input(
            nx,
            ny,
            wheel_amount(delta_x),
            position | MOUSEEVENTF_HWHEEL,
        ));
    }
    send_inputs(&inputs)
}

/// Protocol pixel delta → signed wheel data, never rounding a nonzero request
/// down to nothing.
pub(super) fn wheel_amount(delta_pixels: i32) -> i32 {
    if delta_pixels == 0 {
        return 0;
    }
    let notches = f64::from(delta_pixels) / WHEEL_PIXELS_PER_NOTCH;
    let amount = (notches * WHEEL_DELTA).round() as i32;
    if amount == 0 {
        delta_pixels.signum() * WHEEL_DELTA as i32
    } else {
        amount
    }
}

pub(super) fn type_chunk(units: &[u16]) -> Result<(), ComputerUseError> {
    let mut inputs = Vec::with_capacity(units.len() * 2);
    for &unit in units {
        inputs.push(key_input(0, unit, KEYEVENTF_UNICODE));
        inputs.push(key_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
    }
    send_inputs(&inputs)
}

pub(super) fn key_press(key: &str, modifiers: &[KeyModifier]) -> Result<(), ComputerUseError> {
    let (vk, extended) = key_code(key).ok_or_else(|| {
        ComputerUseError::new(ErrorCode::InvalidRequest, "unsupported key name", false)
    })?;
    let modifier_vks = modifiers
        .iter()
        .map(|modifier| modifier_vk(*modifier))
        .collect::<Result<Vec<_>, _>>()?;

    let mut inputs = Vec::with_capacity(modifier_vks.len() * 2 + 2);
    for &modifier in &modifier_vks {
        inputs.push(key_input(modifier, 0, 0));
    }
    let key_flags = if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
    inputs.push(key_input(vk, 0, key_flags));
    inputs.push(key_input(vk, 0, key_flags | KEYEVENTF_KEYUP));
    for &modifier in modifier_vks.iter().rev() {
        inputs.push(key_input(modifier, 0, KEYEVENTF_KEYUP));
    }
    send_inputs(&inputs)
}

fn modifier_vk(modifier: KeyModifier) -> Result<u16, ComputerUseError> {
    Ok(match modifier {
        KeyModifier::Command => VK_LWIN,
        KeyModifier::Control => VK_CONTROL,
        KeyModifier::Option => VK_MENU,
        KeyModifier::Shift => VK_SHIFT,
        KeyModifier::Function => {
            return Err(ComputerUseError::new(
                ErrorCode::InvalidRequest,
                "the function modifier is not supported on Windows",
                false,
            ))
        }
    })
}

/// Same key-name vocabulary as the macOS backend. The bool marks extended
/// keys, whose scancode prefix some applications require to disambiguate
/// e.g. arrow keys from the numeric pad.
pub(super) fn key_code(key: &str) -> Option<(u16, bool)> {
    let name = key.to_ascii_lowercase();
    if let Some(character) = single_character_vk(&name) {
        return Some((character, false));
    }
    Some(match name.as_str() {
        "enter" | "return" => (VK_RETURN, false),
        "tab" => (VK_TAB, false),
        "space" => (VK_SPACE, false),
        "backspace" | "delete" => (VK_BACK, false),
        "forward_delete" | "del" => (VK_DELETE, true),
        "insert" => (VK_INSERT, true),
        "escape" | "esc" => (VK_ESCAPE, false),
        "home" => (VK_HOME, true),
        "end" => (VK_END, true),
        "page_up" | "pageup" | "pgup" => (VK_PRIOR, true),
        "page_down" | "pagedown" | "pgdn" => (VK_NEXT, true),
        // Models reach for the DOM/JS spellings as often as the bare ones.
        "left" | "arrowleft" | "arrow_left" | "left_arrow" => (VK_LEFT, true),
        "right" | "arrowright" | "arrow_right" | "right_arrow" => (VK_RIGHT, true),
        "up" | "arrowup" | "arrow_up" | "up_arrow" => (VK_UP, true),
        "down" | "arrowdown" | "arrow_down" | "down_arrow" => (VK_DOWN, true),
        // Modifiers pressed on their own: Alt alone activates the menu bar,
        // which is the standard keyboard route into an application's menus.
        "alt" | "option" | "menu" => (VK_MENU, false),
        "control" | "ctrl" => (VK_CONTROL, false),
        "shift" => (VK_SHIFT, false),
        "win" | "windows" | "super" | "meta" | "command" | "cmd" => (VK_LWIN, true),
        "f1" => (VK_F1, false),
        "f2" => (VK_F2, false),
        "f3" => (VK_F3, false),
        "f4" => (VK_F4, false),
        "f5" => (VK_F5, false),
        "f6" => (VK_F6, false),
        "f7" => (VK_F7, false),
        "f8" => (VK_F8, false),
        "f9" => (VK_F9, false),
        "f10" => (VK_F10, false),
        "f11" => (VK_F11, false),
        "f12" => (VK_F12, false),
        _ => return None,
    })
}

fn single_character_vk(name: &str) -> Option<u16> {
    let mut characters = name.chars();
    let (character, None) = (characters.next()?, characters.next()) else {
        return single_named_punctuation(name);
    };
    match character {
        'a'..='z' => Some(character.to_ascii_uppercase() as u16),
        '0'..='9' => Some(character as u16),
        '=' => Some(VK_OEM_PLUS),
        '-' => Some(VK_OEM_MINUS),
        '[' => Some(VK_OEM_4),
        ']' => Some(VK_OEM_6),
        '\'' => Some(VK_OEM_7),
        ';' => Some(VK_OEM_1),
        '\\' => Some(VK_OEM_5),
        ',' => Some(VK_OEM_COMMA),
        '/' => Some(VK_OEM_2),
        '.' => Some(VK_OEM_PERIOD),
        '`' => Some(VK_OEM_3),
        _ => None,
    }
}

fn single_named_punctuation(name: &str) -> Option<u16> {
    Some(match name {
        "equal" => VK_OEM_PLUS,
        "minus" => VK_OEM_MINUS,
        "left_bracket" => VK_OEM_4,
        "right_bracket" => VK_OEM_6,
        "quote" => VK_OEM_7,
        "semicolon" => VK_OEM_1,
        "backslash" => VK_OEM_5,
        "comma" => VK_OEM_COMMA,
        "slash" => VK_OEM_2,
        "period" => VK_OEM_PERIOD,
        "grave" => VK_OEM_3,
        _ => return None,
    })
}

fn button_flags(button: MouseButton) -> (u32, u32) {
    match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    }
}

fn foreground_root() -> HWND {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { GetAncestor(foreground, GA_ROOT) }
}

/// Brings the target to the foreground before keyboard/mouse injection.
///
/// `SetForegroundWindow` is allowed to refuse (foreground lock); the
/// `AttachThreadInput` fallback joins the current foreground thread's input
/// queue, which restores the right to steal focus. A target that still is not
/// foreground afterwards fails the action rather than typing into whatever
/// window actually has focus.
pub(super) fn ensure_foreground(hwnd: HWND) -> Result<(), ComputerUseError> {
    if foreground_root() == hwnd {
        return Ok(());
    }
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
    }
    if wait_for_foreground(hwnd) {
        return Ok(());
    }
    let foreground = unsafe { GetForegroundWindow() };
    if !foreground.is_null() {
        let foreground_thread =
            unsafe { GetWindowThreadProcessId(foreground, std::ptr::null_mut()) };
        let current_thread = unsafe { GetCurrentThreadId() };
        if foreground_thread != 0 && foreground_thread != current_thread {
            unsafe {
                AttachThreadInput(current_thread, foreground_thread, 1);
                SetForegroundWindow(hwnd);
                AttachThreadInput(current_thread, foreground_thread, 0);
            }
        }
    }
    if wait_for_foreground(hwnd) {
        return Ok(());
    }
    Err(input_failed(&foreground_failure_message()))
}

/// Names whoever actually holds the foreground so the failure is actionable
/// ("close the fullscreen game") instead of a bare refusal.
fn foreground_failure_message() -> String {
    let blocker = unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_null() {
            None
        } else {
            let mut pid = 0u32;
            GetWindowThreadProcessId(foreground, &mut pid);
            (pid != 0)
                .then(|| super::window::process_name(pid))
                .flatten()
        }
    };
    match blocker {
        Some(name) => format!(
            "could not bring the target window to the foreground (\"{name}\" is holding it)"
        ),
        None => "could not bring the target window to the foreground".into(),
    }
}

fn wait_for_foreground(hwnd: HWND) -> bool {
    let deadline = std::time::Instant::now() + FOREGROUND_WAIT;
    loop {
        if foreground_root() == hwnd {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(FOREGROUND_POLL);
    }
}

fn input_failed(message: &str) -> ComputerUseError {
    ComputerUseError::new(ErrorCode::InputFailed, message, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> VirtualScreen {
        VirtualScreen {
            x: -1920.0,
            y: -240.0,
            width: 3840.0,
            height: 1440.0,
        }
    }

    #[test]
    fn normalization_covers_the_whole_virtual_desktop() {
        let screen = screen();
        assert_eq!(
            screen
                .normalize(Point {
                    x: -1920.0,
                    y: -240.0
                })
                .unwrap(),
            (0, 0)
        );
        assert_eq!(
            screen
                .normalize(Point {
                    x: -1920.0 + 3839.0,
                    y: -240.0 + 1439.0
                })
                .unwrap(),
            (65535, 65535)
        );
        let (mx, my) = screen.normalize(Point { x: 0.0, y: 480.0 }).unwrap();
        assert!((32700..=32820).contains(&mx), "{mx}");
        assert!((32700..=32820).contains(&my), "{my}");
    }

    #[test]
    fn normalization_clamps_and_rejects_non_finite_points() {
        let screen = screen();
        assert_eq!(
            screen
                .normalize(Point {
                    x: -99999.0,
                    y: 99999.0
                })
                .unwrap(),
            (0, 65535)
        );
        assert!(screen
            .normalize(Point {
                x: f64::NAN,
                y: 0.0
            })
            .is_err());
    }

    #[test]
    fn wheel_amounts_scale_pixels_and_never_vanish() {
        assert_eq!(wheel_amount(0), 0);
        assert_eq!(wheel_amount(40), 120);
        assert_eq!(wheel_amount(-40), -120);
        assert_eq!(wheel_amount(80), 240);
        // Small deltas still produce at least one notch in the right direction.
        assert_eq!(wheel_amount(1), 120 / 40);
        assert!(wheel_amount(-1) < 0);
    }

    #[test]
    fn known_keys_map_and_unknown_keys_fail() {
        assert_eq!(key_code("ENTER"), Some((VK_RETURN, false)));
        assert_eq!(key_code("page_down"), Some((VK_NEXT, true)));
        assert_eq!(key_code("a"), Some((b'A' as u16, false)));
        assert_eq!(key_code("7"), Some((b'7' as u16, false)));
        assert_eq!(key_code("semicolon"), Some((VK_OEM_1, false)));
        assert_eq!(key_code(";"), Some((VK_OEM_1, false)));
        assert_eq!(key_code("left"), Some((VK_LEFT, true)));
        assert_eq!(key_code("definitely_unknown"), None);
    }

    /// Names a model actually reached for on a real run and got refused.
    #[test]
    fn arrow_and_modifier_spellings_models_use_are_accepted() {
        for name in ["arrowdown", "arrow_down", "down_arrow", "DOWN"] {
            assert_eq!(key_code(name), Some((VK_DOWN, true)), "{name}");
        }
        for name in ["arrowup", "arrowleft", "arrowright"] {
            assert!(key_code(name).is_some(), "{name}");
        }
        // Alt on its own is the keyboard route into an application menu bar.
        assert_eq!(key_code("alt"), Some((VK_MENU, false)));
        assert_eq!(key_code("option"), Some((VK_MENU, false)));
        assert_eq!(key_code("ctrl"), Some((VK_CONTROL, false)));
        assert_eq!(key_code("shift"), Some((VK_SHIFT, false)));
        assert_eq!(key_code("win"), Some((VK_LWIN, true)));
        assert_eq!(key_code("pgup"), Some((VK_PRIOR, true)));
        assert_eq!(key_code("insert"), Some((VK_INSERT, true)));
    }

    #[test]
    fn function_modifier_is_rejected_and_others_map() {
        assert_eq!(modifier_vk(KeyModifier::Command).unwrap(), VK_LWIN);
        assert_eq!(modifier_vk(KeyModifier::Control).unwrap(), VK_CONTROL);
        assert_eq!(modifier_vk(KeyModifier::Option).unwrap(), VK_MENU);
        assert_eq!(modifier_vk(KeyModifier::Shift).unwrap(), VK_SHIFT);
        assert_eq!(
            modifier_vk(KeyModifier::Function).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }
}
