//! Window capture: `PrintWindow` first, screen `BitBlt` as the fallback.
//!
//! `PrintWindow(PW_RENDERFULLCONTENT)` asks DWM for the window's own surface,
//! so an occluded target still captures correctly. Some hardware-accelerated
//! windows hand back an empty (uniform) surface instead of failing; those fall
//! back to `BitBlt`, which reads the composited screen at the target's bounds
//! and therefore captures whatever overlaps it — a screenshot that is at least
//! honest about what the user sees.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS,
    HBITMAP, HDC, SRCCOPY,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;

use super::window;
use crate::runtime::{ComputerUseError, ErrorCode, Rect};

/// Not in every SDK's headers; renders the DWM-composited content (needed for
/// Chromium/Electron and other DirectComposition windows).
const PW_RENDERFULLCONTENT: u32 = 2;

pub(super) struct CapturedFrame {
    /// Tightly packed RGBA8.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Captures `region` — normally the target's frame, and the frame united with
/// its open menus when it has any.
///
/// `screen_only` skips `PrintWindow` entirely. Dropdown menus, context menus
/// and combo-box lists are separate top-level windows, so printing the target
/// never contains them — a model asking "what is in this menu?" would get a
/// screenshot of the window with no menu in it. Reading the composited screen
/// is the only way to show what the user actually sees, and it is also the
/// only way to cover a region that reaches outside the window.
pub(super) fn capture_region(
    hwnd: HWND,
    region: Rect,
    screen_only: bool,
) -> Result<CapturedFrame, ComputerUseError> {
    let region_rect = screen_rect(region).ok_or_else(capture_failed)?;
    let region_width = region_rect.right - region_rect.left;
    let region_height = region_rect.bottom - region_rect.top;

    if screen_only {
        let surface = Surface::new(region_width, region_height)?;
        if surface.blit_screen(&region_rect, region_width, region_height) {
            let mut pixels = surface.read_pixels(region_width, region_height);
            bgra_to_rgba(&mut pixels);
            return Ok(CapturedFrame {
                pixels,
                width: region_width as u32,
                height: region_height as u32,
            });
        }
        // Fall through: a window print without its menu beats no screenshot.
    }

    let window_rect = window::window_rect(hwnd).ok_or_else(capture_failed)?;
    let width = window_rect.right.saturating_sub(window_rect.left);
    let height = window_rect.bottom.saturating_sub(window_rect.top);
    if width <= 0 || height <= 0 {
        return Err(capture_failed());
    }

    let surface = Surface::new(width, height)?;
    let crop = crop_within(region, window_rect, width, height);
    let read_cropped = |surface: &Surface| {
        let bgra = surface.read_pixels(width, height);
        crop_bgra(&bgra, width as u32, crop)
    };

    let printed = unsafe { PrintWindow(hwnd, surface.dc, PW_RENDERFULLCONTENT) } != 0;
    unsafe { GdiFlush() };
    let printed_pixels = printed.then(|| read_cropped(&surface));
    // A window that "prints" as one flat color almost certainly refused to
    // render (GPU swap-chain content). Re-read the composited screen.
    let needs_screen = !printed_pixels
        .as_ref()
        .is_some_and(|pixels| !is_uniform(pixels));
    let cropped = match (
        needs_screen,
        surface.blit_screen(&window_rect, width, height),
    ) {
        (true, true) => Some(read_cropped(&surface)),
        // Falling back failed: a flat print beats no screenshot at all.
        (_, _) => printed_pixels,
    };

    let mut cropped = cropped.ok_or_else(capture_failed)?;
    bgra_to_rgba(&mut cropped);
    Ok(CapturedFrame {
        pixels: cropped,
        width: crop.width,
        height: crop.height,
    })
}

/// Rounds a logical rect to whole physical pixels, rejecting degenerate ones.
fn screen_rect(region: Rect) -> Option<RECT> {
    if !region.is_valid() {
        return None;
    }
    let left = region.x.round() as i32;
    let top = region.y.round() as i32;
    let right = (region.x + region.width).round() as i32;
    let bottom = (region.y + region.height).round() as i32;
    (right > left && bottom > top).then_some(RECT {
        left,
        top,
        right,
        bottom,
    })
}

struct Surface {
    screen_dc: HDC,
    dc: HDC,
    bitmap: HBITMAP,
    previous: *mut c_void,
    bits: *mut c_void,
    length: usize,
}

impl Surface {
    fn new(width: i32, height: i32) -> Result<Self, ComputerUseError> {
        unsafe {
            let screen_dc = GetDC(std::ptr::null_mut());
            if screen_dc.is_null() {
                return Err(capture_failed());
            }
            let dc = CreateCompatibleDC(screen_dc);
            if dc.is_null() {
                ReleaseDC(std::ptr::null_mut(), screen_dc);
                return Err(capture_failed());
            }
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative height: top-down rows, matching image coordinates.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(
                screen_dc,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            if bitmap.is_null() || bits.is_null() {
                DeleteDC(dc);
                ReleaseDC(std::ptr::null_mut(), screen_dc);
                return Err(capture_failed());
            }
            let previous = SelectObject(dc, bitmap);
            Ok(Self {
                screen_dc,
                dc,
                bitmap,
                previous,
                bits,
                length: width as usize * height as usize * 4,
            })
        }
    }

    fn read_pixels(&self, width: i32, height: i32) -> Vec<u8> {
        debug_assert_eq!(self.length, width as usize * height as usize * 4);
        unsafe { std::slice::from_raw_parts(self.bits.cast::<u8>(), self.length).to_vec() }
    }

    /// Reads the composited desktop at the window's position, which includes
    /// anything drawn on top of it — menus above all.
    fn blit_screen(&self, window_rect: &RECT, width: i32, height: i32) -> bool {
        let copied = unsafe {
            BitBlt(
                self.dc,
                0,
                0,
                width,
                height,
                self.screen_dc,
                window_rect.left,
                window_rect.top,
                SRCCOPY | CAPTUREBLT,
            )
        } != 0;
        if copied {
            unsafe { GdiFlush() };
        }
        copied
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
            ReleaseDC(std::ptr::null_mut(), self.screen_dc);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The extended-frame bounds expressed inside the captured window rect,
/// clamped so a frame that protrudes past the rect can never over-read.
pub(super) fn crop_within(frame: Rect, window_rect: RECT, width: i32, height: i32) -> CropRect {
    let x = ((frame.x.round() as i64) - i64::from(window_rect.left)).clamp(0, i64::from(width));
    let y = ((frame.y.round() as i64) - i64::from(window_rect.top)).clamp(0, i64::from(height));
    let crop_width = (frame.width.round() as i64).clamp(0, i64::from(width) - x);
    let crop_height = (frame.height.round() as i64).clamp(0, i64::from(height) - y);
    if crop_width == 0 || crop_height == 0 {
        return CropRect {
            x: 0,
            y: 0,
            width: width.max(1) as u32,
            height: height.max(1) as u32,
        };
    }
    CropRect {
        x: x as u32,
        y: y as u32,
        width: crop_width as u32,
        height: crop_height as u32,
    }
}

pub(super) fn crop_bgra(pixels: &[u8], source_width: u32, crop: CropRect) -> Vec<u8> {
    let mut output = Vec::with_capacity(crop.width as usize * crop.height as usize * 4);
    let stride = source_width as usize * 4;
    for row in 0..crop.height as usize {
        let start = (crop.y as usize + row) * stride + crop.x as usize * 4;
        let end = start + crop.width as usize * 4;
        output.extend_from_slice(&pixels[start..end]);
    }
    output
}

fn bgra_to_rgba(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        // GDI leaves alpha undefined for opaque captures; force it.
        pixel[3] = 0xFF;
    }
}

pub(super) fn is_uniform(pixels: &[u8]) -> bool {
    let Some(first) = pixels.get(..4) else {
        return true;
    };
    pixels.chunks_exact(4).all(|pixel| pixel == first)
}

fn capture_failed() -> ComputerUseError {
    ComputerUseError::new(
        ErrorCode::CaptureFailed,
        "failed to capture the target window",
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_maps_frame_into_window_rect_and_clamps() {
        let window_rect = RECT {
            left: 100,
            top: 200,
            right: 500,
            bottom: 500,
        };
        let crop = crop_within(
            Rect {
                x: 107.0,
                y: 200.0,
                width: 386.0,
                height: 293.0,
            },
            window_rect,
            400,
            300,
        );
        assert_eq!(
            crop,
            CropRect {
                x: 7,
                y: 0,
                width: 386,
                height: 293
            }
        );

        // A frame extending past the captured rect must clamp, not over-read.
        let oversized = crop_within(
            Rect {
                x: 90.0,
                y: 190.0,
                width: 1000.0,
                height: 1000.0,
            },
            window_rect,
            400,
            300,
        );
        assert!(oversized.x + oversized.width <= 400);
        assert!(oversized.y + oversized.height <= 300);
    }

    #[test]
    fn cropping_extracts_the_requested_rows() {
        // 3x2 image, pixel value = column index in every byte.
        let mut pixels = Vec::new();
        for _row in 0..2 {
            for column in 0..3u8 {
                pixels.extend_from_slice(&[column; 4]);
            }
        }
        let cropped = crop_bgra(
            &pixels,
            3,
            CropRect {
                x: 1,
                y: 0,
                width: 2,
                height: 2,
            },
        );
        assert_eq!(cropped.len(), 2 * 2 * 4);
        assert_eq!(&cropped[..4], &[1; 4]);
        assert_eq!(&cropped[4..8], &[2; 4]);
    }

    #[test]
    fn uniform_surfaces_are_detected_as_capture_refusals() {
        assert!(is_uniform(&[0, 0, 0, 255, 0, 0, 0, 255]));
        assert!(!is_uniform(&[0, 0, 0, 255, 1, 0, 0, 255]));
        assert!(is_uniform(&[]));
    }

    #[test]
    fn bgra_conversion_swaps_channels_and_forces_alpha() {
        let mut pixels = vec![10, 20, 30, 0];
        bgra_to_rgba(&mut pixels);
        assert_eq!(pixels, vec![30, 20, 10, 255]);
    }
}
