use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 2;
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
pub const MAX_TYPE_CHARS: usize = 16_384;
pub const MAX_WAIT_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn contains_screen_point(self, point: Point) -> bool {
        finite(point.x)
            && finite(point.y)
            && point.x >= self.x
            && point.y >= self.y
            && point.x < self.x + self.width
            && point.y < self.y + self.height
    }

    pub fn target_point(self, x: f64, y: f64) -> Result<Point, ComputerUseError> {
        if !self.is_valid() || !finite(x) || !finite(y) || x < 0.0 || y < 0.0 {
            return Err(ComputerUseError::invalid_coordinates());
        }
        if x >= self.width || y >= self.height {
            return Err(ComputerUseError::invalid_coordinates());
        }
        Ok(Point {
            x: self.x + x,
            y: self.y + y,
        })
    }

    pub fn is_valid(self) -> bool {
        finite(self.x)
            && finite(self.y)
            && finite(self.width)
            && finite(self.height)
            && self.width > 0.0
            && self.height > 0.0
    }
}

fn finite(value: f64) -> bool {
    value.is_finite()
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyModifier {
    Command,
    Control,
    Option,
    Shift,
    Function,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Observe {
        /// Screen point used only to acquire a target. Omit after lock-on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Point>,
    },
    Click {
        x: f64,
        y: f64,
        #[serde(default)]
        button: MouseButton,
    },
    DoubleClick {
        x: f64,
        y: f64,
        #[serde(default)]
        button: MouseButton,
    },
    Move {
        x: f64,
        y: f64,
    },
    Scroll {
        x: f64,
        y: f64,
        delta_x: i32,
        delta_y: i32,
    },
    Type {
        text: String,
    },
    Key {
        key: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        modifiers: Vec<KeyModifier>,
    },
    Wait {
        duration_ms: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetWindow {
    /// Informational only. The request schema has no corresponding ID field.
    pub id: u64,
    pub owner_pid: u32,
    pub owner_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub frame: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    Unknown,
    Denied,
    Granted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionStatus {
    pub screen_recording: PermissionState,
    pub accessibility: PermissionState,
}

impl Default for PermissionStatus {
    fn default() -> Self {
        Self {
            screen_recording: PermissionState::Unknown,
            accessibility: PermissionState::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    WaitingForTarget,
    Active,
    Paused,
    Stopped,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusResponse {
    pub state: ServiceState,
    pub permissions: PermissionStatus,
    pub target_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetWindow>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotResponse {
    /// RFC 4648 base64 of a PNG containing only the locked target window.
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionResponse {
    pub status: StatusResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<ScreenshotResponse>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub version: u16,
    pub id: u64,
    pub token: String,
    pub request: Request,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Action {
        action: Action,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_epoch: Option<u64>,
    },
    Status,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    pub version: u16,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ActionResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ComputerUseError>,
}

impl ResponseEnvelope {
    pub fn success(id: u64, result: ActionResponse) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: u64, error: ComputerUseError) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unsupported,
    Unauthorized,
    InvalidRequest,
    InvalidCoordinates,
    PermissionDenied,
    TargetNotSelected,
    TargetInvalid,
    CaptureFailed,
    InputFailed,
    ProtocolMismatch,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerUseError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ComputerUseError {
    pub fn new(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }

    pub fn unsupported() -> Self {
        Self::new(
            ErrorCode::Unsupported,
            "native Computer Use is only supported on macOS",
            false,
        )
    }

    pub fn invalid_coordinates() -> Self {
        Self::new(
            ErrorCode::InvalidCoordinates,
            "coordinates must be finite and strictly inside the target window",
            false,
        )
    }
}

impl std::fmt::Display for ComputerUseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ComputerUseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_round_trip_covers_all_variants() {
        let actions = [
            Action::Observe {
                target: Some(Point { x: 12.0, y: 34.0 }),
            },
            Action::Click {
                x: 1.0,
                y: 2.0,
                button: MouseButton::Left,
            },
            Action::DoubleClick {
                x: 2.0,
                y: 3.0,
                button: MouseButton::Right,
            },
            Action::Move { x: 4.0, y: 5.0 },
            Action::Scroll {
                x: 8.0,
                y: 9.0,
                delta_x: -6,
                delta_y: 7,
            },
            Action::Type {
                text: "hello".into(),
            },
            Action::Key {
                key: "enter".into(),
                modifiers: vec![KeyModifier::Command],
            },
            Action::Wait { duration_ms: 25 },
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
        }
    }

    #[test]
    fn request_schema_has_no_window_id_and_rejects_unknown_fields() {
        let json = r#"{"version":1,"id":7,"token":"x","request":{"type":"action","action":{"type":"observe","window_id":42}}}"#;
        assert!(serde_json::from_str::<RequestEnvelope>(json).is_err());
    }

    #[test]
    fn coordinates_are_strict_and_finite() {
        let rect = Rect {
            x: 100.0,
            y: 50.0,
            width: 20.0,
            height: 10.0,
        };
        assert_eq!(
            rect.target_point(0.0, 0.0).unwrap(),
            Point { x: 100.0, y: 50.0 }
        );
        assert!(rect.target_point(19.999, 9.999).is_ok());
        assert!(rect.target_point(20.0, 0.0).is_err());
        assert!(rect.target_point(0.0, 10.0).is_err());
        assert!(rect.target_point(-0.1, 0.0).is_err());
        assert!(rect.target_point(f64::NAN, 0.0).is_err());
        assert!(!rect.contains_screen_point(Point { x: 120.0, y: 55.0 }));
    }

    #[test]
    fn malformed_or_extra_input_is_rejected() {
        assert!(
            serde_json::from_str::<Action>(r#"{"type":"wait","duration_ms":1,"extra":true}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<Action>(r#"{"type":"click","x":"no","y":2}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"type":"stop"}"#).is_err());
    }
}
