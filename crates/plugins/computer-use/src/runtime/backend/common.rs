//! What both native backends do the same way.
//!
//! macOS and Windows drive completely different window systems, but a
//! few answers do not depend on which: whether a capture still matches
//! the frame it was taken from, how a screenshot is bounded before it is
//! sent, and the four errors that mean the same thing on either side.
//! Each of these existed twice, byte for byte, and drift between the two
//! copies would be a difference nobody asked for.

use crate::runtime::{ComputerUseError, ErrorCode, Rect};

const MAX_SCREENSHOT_EDGE: u32 = 1_600;

pub(super) fn capture_frame_matches(captured: Option<Rect>, frame: Rect) -> bool {
    captured.is_some_and(|captured| {
        (frame.x - captured.x).abs() <= 0.5
            && (frame.y - captured.y).abs() <= 0.5
            && (frame.width - captured.width).abs() <= 0.5
            && (frame.height - captured.height).abs() <= 0.5
    })
}

pub(super) fn bounded_dimensions(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= MAX_SCREENSHOT_EDGE {
        return (width, height);
    }
    let scale = u64::from(MAX_SCREENSHOT_EDGE);
    let longest = u64::from(longest);
    let resize =
        |dimension: u32| ((u64::from(dimension) * scale + longest / 2) / longest).max(1) as u32;
    (resize(width), resize(height))
}

pub(super) fn action_cancelled() -> ComputerUseError {
    ComputerUseError::new(
        ErrorCode::TargetInvalid,
        "Computer Use action was cancelled",
        true,
    )
}

pub(super) fn marker_error(error: std::io::Error) -> ComputerUseError {
    ComputerUseError::new(
        ErrorCode::Internal,
        format!("failed to update Computer Use activation marker: {error}"),
        false,
    )
}

pub(super) fn invalid_input(message: &str) -> ComputerUseError {
    ComputerUseError::new(ErrorCode::InvalidRequest, message, false)
}

pub(super) fn capture_dimensions_error() -> ComputerUseError {
    ComputerUseError::new(
        ErrorCode::CaptureFailed,
        "target screenshot has invalid dimensions",
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moved_or_resized_targets_require_a_fresh_observation() {
        let frame = Rect {
            x: 10.0,
            y: 20.0,
            width: 800.0,
            height: 600.0,
        };
        assert!(capture_frame_matches(Some(frame), frame));
        assert!(!capture_frame_matches(
            Some(Rect { x: 9.0, ..frame }),
            frame
        ));
        assert!(!capture_frame_matches(
            Some(Rect {
                width: 700.0,
                ..frame
            }),
            frame
        ));
        assert!(!capture_frame_matches(None, frame));
    }

    #[test]
    fn screenshot_dimensions_are_bounded_without_upscaling() {
        assert_eq!(bounded_dimensions(800, 600), (800, 600));
        assert_eq!(bounded_dimensions(3_200, 2_000), (1_600, 1_000));
        assert_eq!(bounded_dimensions(2_000, 3_200), (1_000, 1_600));
    }
}
